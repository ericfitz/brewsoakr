use crate::nosoak::{self, NoSoakList};
use crate::settings;
use crate::{Error, SoakHours};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use toml_edit::DocumentMut;

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
    /// Always printed to stderr (not only under `-v`).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Resolved `SOAK_HOURS`: CLI > env > file > 24.
    pub hours: SoakHours,
    pub persist: PersistAction,
    pub taps: Vec<TapEntry>,
    pub no_soak: NoSoakList,
    pub notes: Vec<String>,
    /// Always printed to stderr (not only under `-v`).
    pub warnings: Vec<String>,
}

impl Config {
    pub fn uniform(hours: SoakHours) -> Self {
        Self {
            hours,
            persist: PersistAction::None,
            taps: Vec::new(),
            no_soak: NoSoakList::default(),
            notes: Vec::new(),
            warnings: Vec::new(),
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
        warnings: parsed.warnings,
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
        if entry.get("NO_SOAK").is_some() {
            let label = entry
                .get("name")
                .and_then(toml::Value::as_str)
                .unwrap_or("(unnamed)");
            out.warnings.push(format!(
                "config: NO_SOAK inside [[TAP]] {label} is ignored; move it above the first [[TAP]] table; run brewsoak settings repair"
            ));
        }
        let Some(name) = entry.get("name").and_then(toml::Value::as_str) else {
            out.notes.push(
                "config: [[TAP]] entry is missing name; skipped; run brewsoak settings repair"
                    .into(),
            );
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
            out.notes.push(format!(
                "config: [[TAP]] name {name:?} is not user/repo; skipped; run brewsoak settings repair"
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
                        "config: [[TAP]] {lower} soak_hours {h} is not an integer >= 1; using SOAK_HOURS; run brewsoak settings repair"
                    ));
                    None
                }
            },
        };
        if let Some(pos) = out.taps.iter().position(|t| t.name == lower) {
            out.notes.push(format!(
                "config: duplicate [[TAP]] {lower}; last entry wins; run brewsoak settings repair"
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
    let Some(array) = raw.as_array() else {
        let hint = if raw.as_str().is_some_and(|s| nosoak::parse_entry(s).is_ok()) {
            "; run brewsoak settings repair"
        } else {
            ""
        };
        out.notes.push(format!(
            "config: NO_SOAK is not an array of strings; ignored{hint}"
        ));
        return;
    };
    let strings: Option<Vec<&str>> = array.iter().map(toml::Value::as_str).collect();
    let Some(strings) = strings else {
        out.notes.push(
            "config: NO_SOAK contains non-string entries; ignored; run brewsoak settings repair"
                .into(),
        );
        return;
    };
    let mut entries = Vec::new();
    for s in strings {
        match nosoak::parse_entry(s) {
            Ok(e) => entries.push(e),
            Err(reason) => out.notes.push(format!(
                "config: {}; run brewsoak settings repair",
                reason.trim_end_matches("; skipped")
            )),
        }
    }
    out.no_soak = NoSoakList::new(entries);
}

pub fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// `None` when the file does not exist; any other I/O error is returned.
pub fn read_existing(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Backups kept after every write; older `.bak` files are deleted.
const BACKUPS_KEPT: usize = 2;

/// A temp file this much older than now is a failed run's leftover.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// `20231114T221320Z`: UTC, sortable, safe in a file name.
pub(crate) fn backup_stamp(now: OffsetDateTime) -> String {
    let format =
        time::format_description::parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")
            .expect("static format description");
    now.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .expect("a UTC datetime formats")
}

/// `config.toml.20231114T221320Z-2.bak` -> `("20231114T221320Z", 2)`; the
/// bare name is suffix 1. The stamp must be exactly `YYYYMMDDTHHMMSSZ`;
/// anything else (`config.toml.old.bak`) is not a backup of `name`.
pub(crate) fn backup_key(name: &str, file: &str) -> Option<(String, u32)> {
    let middle = file
        .strip_prefix(name)?
        .strip_prefix('.')?
        .strip_suffix(".bak")?;
    let (stamp, n) = match middle.split_once('-') {
        None => (middle, 1),
        Some((stamp, n)) => (stamp, n.parse().ok()?),
    };
    let b = stamp.as_bytes();
    let shaped = b.len() == 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z';
    shaped.then(|| (stamp.to_string(), n))
}

fn backup_name(name: &str, stamp: &str, n: u32) -> String {
    if n == 1 {
        format!("{name}.{stamp}.bak")
    } else {
        format!("{name}.{stamp}-{n}.bak")
    }
}

/// Spec "config::write_atomic". Readers see the old file or the new one,
/// never none: the body goes to a temp file, the current file is hard-linked
/// to a `.bak`, and the temp file is renamed over it.
///
/// There is no lock. Two overlapping runs each rename atomically and the
/// last one wins; both succeed. The stale-temp sweep at the end removes only
/// temp files more than ten minutes old, so it never deletes the other
/// run's in-flight temp file.
pub fn write_atomic(path: &Path, new_body: &str, now: OffsetDateTime) -> Result<(), Error> {
    let current = read_existing(path)?;
    if current.as_deref().unwrap_or("") == new_body {
        return Ok(());
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| {
            Error::Other(format!(
                "config: {} has no parent directory",
                path.display()
            ))
        })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Other(format!("config: {} has no file name", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp_prefix = format!(".{name}.tmp-");
    let tmp = dir.join(format!("{tmp_prefix}{}", std::process::id()));
    let result = write_temp(&tmp, new_body)
        .map_err(Error::from)
        .and_then(|()| replace_with_temp(path, &tmp, dir, name, new_body, current.is_some(), now));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    prune_backups(dir, name)?;
    remove_stale_temps(dir, &tmp_prefix, &tmp)
}

/// Mode 0600 and fsync. `create` + `truncate` (not `create_new`) so a
/// leftover from an earlier run with the same pid does not block the write;
/// `set_permissions` because `mode()` applies only when the file is created.
fn write_temp(tmp: &Path, body: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

/// Steps 4 and 5: back up the current file, then rename the temp file over
/// it, or remove both when the new body is only whitespace.
fn replace_with_temp(
    path: &Path,
    tmp: &Path,
    dir: &Path,
    name: &str,
    new_body: &str,
    exists: bool,
    now: OffsetDateTime,
) -> Result<(), Error> {
    if exists {
        make_backup(path, dir, name, now)?;
    }
    if new_body.trim().is_empty() {
        std::fs::remove_file(tmp)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        std::fs::rename(tmp, path).map_err(Error::from)
    }
}

/// Hard link to `<name>.<stamp>.bak`, or `-N` with N one above the highest
/// suffix already used for this second, so the newest backup always sorts
/// last and pruning never deletes it. A link onto an existing name fails
/// atomically, so two runs in the same second cannot share a backup: on
/// `AlreadyExists` the next suffix is tried. Copy when linking is not
/// possible.
fn make_backup(path: &Path, dir: &Path, name: &str, now: OffsetDateTime) -> Result<(), Error> {
    let stamp = backup_stamp(now);
    let mut highest = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(file) = entry.file_name().to_str()
            && let Some((s, n)) = backup_key(name, file)
            && s == stamp
        {
            highest = highest.max(n);
        }
    }
    for n in (highest + 1)..=999 {
        let backup = dir.join(backup_name(name, &stamp, n));
        match std::fs::hard_link(path, &backup) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                std::fs::copy(path, &backup)?;
                return Ok(());
            }
        }
    }
    Err(Error::Other(format!(
        "config: more than 999 backups of {name} in one second; not writing"
    )))
}

/// Step 6: keep the newest `BACKUPS_KEPT` by `(stamp, suffix)`.
fn prune_backups(dir: &Path, name: &str) -> Result<(), Error> {
    let mut backups: Vec<((String, u32), PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        if let Some(key) = backup_key(name, file) {
            backups.push((key, entry.path()));
        }
    }
    backups.sort();
    let excess = backups.len().saturating_sub(BACKUPS_KEPT);
    for (_, old) in backups.into_iter().take(excess) {
        std::fs::remove_file(old)?;
    }
    Ok(())
}

/// Step 7: `.config.toml.tmp-*` left by earlier failed runs. Only files
/// older than `STALE_TEMP_AGE` go; a younger one may belong to a run that is
/// still writing. Our own temp file is already renamed or removed; skip it
/// anyway. A file that vanishes mid-sweep (another run finished) is fine.
fn remove_stale_temps(dir: &Path, prefix: &str, own: &Path) -> Result<(), Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let is_temp = entry
            .file_name()
            .to_str()
            .is_some_and(|f| f.starts_with(prefix));
        if !is_temp || entry.path() == own {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok());
        if age.is_some_and(|a| a > STALE_TEMP_AGE) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Key-level edit of `SOAK_HOURS` through the settings editor, so comments
/// and unknown keys survive, written with `write_atomic`. Returns a warning
/// instead of touching a file that is not valid TOML.
pub fn apply_persist(
    action: PersistAction,
    path: &Path,
    now: OffsetDateTime,
) -> Result<Option<String>, Error> {
    if action == PersistAction::None {
        return Ok(None);
    }
    let existing = read_existing(path)?;
    let mut doc = match existing.as_deref() {
        None => DocumentMut::new(),
        Some(s) => match s.parse::<DocumentMut>() {
            Ok(doc) => doc,
            Err(_) => {
                return Ok(Some(format!(
                    "{} is not valid TOML; --soak-hours was not persisted (it still applies to this run)",
                    path.display()
                )));
            }
        },
    };
    let edit = match action {
        PersistAction::None => return Ok(None),
        PersistAction::Write(hours) => settings::set_soak_hours(&mut doc, hours),
        PersistAction::Delete => settings::clear_soak_hours(&mut doc),
    };
    if edit.changed {
        write_atomic(path, &doc.to_string(), now)?;
    }
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

    // `OffsetDateTime`, `Path` and `PermissionsExt` come in through
    // `use super::*`.
    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("fixed now")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn age_file(path: &Path, secs: u64) {
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }

    #[test]
    fn backup_stamp_is_utc_compact_iso() {
        assert_eq!(backup_stamp(at(0)), "20231114T221320Z");
    }

    #[test]
    fn backup_key_orders_same_second_suffixes_after_the_bare_name() {
        let k = |f| backup_key("config.toml", f);
        assert_eq!(
            k("config.toml.20231114T221320Z.bak"),
            Some(("20231114T221320Z".into(), 1))
        );
        assert_eq!(
            k("config.toml.20231114T221320Z-2.bak"),
            Some(("20231114T221320Z".into(), 2))
        );
        assert!(k("config.toml.20231114T221320Z-2.bak") > k("config.toml.20231114T221320Z.bak"));
        assert!(k("config.toml.20231114T221320Z-10.bak") > k("config.toml.20231114T221320Z-2.bak"));
        assert!(k("config.toml.20231114T221321Z.bak") > k("config.toml.20231114T221320Z-10.bak"));
        assert_eq!(k("config.toml"), None);
        assert_eq!(k(".config.toml.tmp-1"), None);
        assert_eq!(k("config.toml.20231114T221320Z-x.bak"), None);
        assert_eq!(k("other.toml.20231114T221320Z.bak"), None);
    }

    #[test]
    fn unchanged_body_makes_no_write_and_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(
            names(dir.path()),
            ["config.toml"],
            "first write of a new file has nothing to back up"
        );
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_atomic(&path, "a\n", at(1)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"]);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        write_atomic(&path, "b\n", at(2)).unwrap();
        assert_eq!(
            names(dir.path()),
            ["config.toml", "config.toml.20231114T221322Z.bak"]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.20231114T221322Z.bak")).unwrap(),
            "a\n"
        );
        write_atomic(&path, "b\n", at(3)).unwrap();
        assert_eq!(
            names(dir.path()).len(),
            2,
            "second identical edit: no backup"
        );
    }

    #[test]
    fn backups_keep_the_two_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (i, body) in ["a\n", "b\n", "c\n", "d\n"].iter().enumerate() {
            write_atomic(&path, body, at(i as i64)).unwrap();
        }
        assert_eq!(
            names(dir.path()),
            [
                "config.toml",
                "config.toml.20231114T221322Z.bak",
                "config.toml.20231114T221323Z.bak"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.20231114T221322Z.bak")).unwrap(),
            "b\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.20231114T221323Z.bak")).unwrap(),
            "c\n"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "d\n");
    }

    #[test]
    fn same_second_backups_get_suffixes_and_prune_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bodies = ["a\n", "b\n", "c\n", "d\n", "e\n", "f\n", "g\n", "h\n"];
        write_atomic(&path, bodies[0], at(0)).unwrap();
        for (i, body) in bodies.iter().enumerate().skip(1) {
            write_atomic(&path, body, at(0)).unwrap();
            let mut backups: Vec<((String, u32), String)> = names(dir.path())
                .into_iter()
                .filter_map(|f| backup_key("config.toml", &f).map(|k| (k, f)))
                .collect();
            backups.sort();
            assert_eq!(backups.len(), i.min(2), "write {i}: {backups:?}");
            let newest = &backups.last().unwrap().1;
            assert_eq!(
                std::fs::read_to_string(dir.path().join(newest)).unwrap(),
                bodies[i - 1],
                "write {i}: newest backup {newest} holds the previous body"
            );
        }
    }

    #[test]
    fn files_that_only_look_like_backups_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for junk in ["config.toml.old.bak", "config.toml.2026.bak"] {
            std::fs::write(dir.path().join(junk), "junk").unwrap();
        }
        for (i, body) in ["a\n", "b\n", "c\n", "d\n"].iter().enumerate() {
            write_atomic(&path, body, at(i as i64)).unwrap();
        }
        assert_eq!(
            names(dir.path()),
            [
                "config.toml",
                "config.toml.20231114T221322Z.bak",
                "config.toml.20231114T221323Z.bak",
                "config.toml.2026.bak",
                "config.toml.old.bak"
            ]
        );
        assert_eq!(backup_key("config.toml", "config.toml.old.bak"), None);
        assert_eq!(backup_key("config.toml", "config.toml.2026.bak"), None);
    }

    #[test]
    fn empty_body_removes_the_file_and_keeps_a_backup() {
        for empty in ["", "  \n"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            write_atomic(&path, "a\n", at(0)).unwrap();
            write_atomic(&path, empty, at(1)).unwrap();
            assert!(!path.exists(), "{empty:?}");
            assert_eq!(
                names(dir.path()),
                ["config.toml.20231114T221321Z.bak"],
                "{empty:?}"
            );
            write_atomic(&path, "", at(2)).unwrap();
            assert_eq!(names(dir.path()).len(), 1, "empty onto missing is a no-op");
        }
    }

    #[test]
    fn comment_only_body_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, "# keep\n", at(0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# keep\n");
    }

    #[test]
    fn missing_directory_is_created_and_mode_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x/y/config.toml");
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn stale_temp_files_are_removed_and_fresh_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let stale = dir.path().join(".config.toml.tmp-1");
        let fresh = dir.path().join(".config.toml.tmp-2");
        std::fs::write(&stale, "junk").unwrap();
        std::fs::write(&fresh, "live").unwrap();
        age_file(&stale, 11 * 60);
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(names(dir.path()), [".config.toml.tmp-2", "config.toml"]);
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "live");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[test]
    fn leftover_temp_with_our_own_pid_does_not_block_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let own = dir
            .path()
            .join(format!(".config.toml.tmp-{}", std::process::id()));
        std::fs::write(&own, "junk").unwrap();
        write_atomic(&path, "a\n", at(0)).unwrap();
        assert_eq!(names(dir.path()), ["config.toml"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[test]
    fn read_existing_distinguishes_missing_from_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(read_existing(&path).unwrap(), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_existing(&path).unwrap(), Some(String::new()));
    }

    #[test]
    fn apply_write_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        apply_persist(
            PersistAction::Write(SoakHours::new(48).unwrap()),
            &path,
            at(0),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SOAK_HOURS = 48\n");
        apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(!path.exists());
        assert_eq!(
            names(dir.path()),
            ["config.toml.20231114T221321Z.bak"],
            "the delete left a backup"
        );
        apply_persist(PersistAction::Delete, &path, at(2)).unwrap(); // missing is ok
        assert_eq!(
            names(dir.path()).len(),
            1,
            "nothing to do: no write, no backup"
        );
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
        assert_eq!(
            p.notes,
            [
                "config: NO_SOAK entry \"a/b/c/d\" has more than two slashes; run brewsoak settings repair",
                "config: NO_SOAK entry is empty; run brewsoak settings repair",
            ]
        );
    }

    #[test]
    fn lone_no_soak_string_points_at_repair_only_when_repair_can_fix_it() {
        let p = parse_file("NO_SOAK = \"wget\"\n");
        assert_eq!(
            p.notes,
            ["config: NO_SOAK is not an array of strings; ignored; run brewsoak settings repair"]
        );
        for bad in ["NO_SOAK = \"a//b\"\n", "NO_SOAK = 3\n"] {
            let p = parse_file(bad);
            assert_eq!(
                p.notes,
                ["config: NO_SOAK is not an array of strings; ignored"]
            );
        }
    }

    #[test]
    fn tap_problems_point_at_repair() {
        let p = parse_file(
            "[[TAP]]\nsoak_hours = 1\n\n[[TAP]]\nname = \"x\"\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\nNO_SOAK = [\"w\"]\n\n[[TAP]]\nname = \"a/b\"\n",
        );
        assert_eq!(p.notes.len(), 4, "{:?}", p.notes);
        assert!(
            p.notes
                .iter()
                .chain(&p.warnings)
                .all(|n| n.ends_with("; run brewsoak settings repair")),
            "{:?} {:?}",
            p.notes,
            p.warnings
        );
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn no_soak_with_non_strings_is_ignored_and_points_at_repair() {
        let p = parse_file("NO_SOAK = [\"wget\", 3]\n");
        assert!(p.no_soak.is_empty());
        assert_eq!(
            p.notes,
            ["config: NO_SOAK contains non-string entries; ignored; run brewsoak settings repair"]
        );
        let p = parse_file("NO_SOAK = \"wget\"\n");
        assert_eq!(
            p.notes,
            ["config: NO_SOAK is not an array of strings; ignored; run brewsoak settings repair"]
        );
    }

    #[test]
    fn no_soak_inside_tap_table_warns_and_is_not_applied() {
        let p = parse_file("[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n");
        assert!(p.no_soak.is_empty());
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("NO_SOAK") && p.warnings[0].contains("a/b"));
        assert!(p.warnings[0].contains("above the first [[TAP]]"));
        assert!(parse_file(FULL).warnings.is_empty());
    }

    #[test]
    fn bad_toml_is_all_defaults() {
        let p = parse_file("[[[");
        assert_eq!(p, ParsedFile::default());
    }

    const COMMENTED: &str = "# keep me\nSOAK_HOURS = 6 # why\nNO_SOAK = [\"wget\"]\n";

    #[test]
    fn persist_write_keeps_other_keys_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        let warn = apply_persist(
            PersistAction::Write(SoakHours::new(12).unwrap()),
            &path,
            at(0),
        )
        .unwrap();
        assert_eq!(warn, None);
        let p = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.soak_hours.map(|h| h.get()), Some(12));
        assert_eq!(p.taps.len(), 2);
        assert!(p.no_soak.matches("ericfitz/tap", "x"));
        std::fs::write(&path, COMMENTED).unwrap();
        apply_persist(
            PersistAction::Write(SoakHours::new(12).unwrap()),
            &path,
            at(1),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# keep me\nSOAK_HOURS = 12 # why\nNO_SOAK = [\"wget\"]\n"
        );
    }

    #[test]
    fn persist_same_value_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED).unwrap();
        apply_persist(
            PersistAction::Write(SoakHours::new(6).unwrap()),
            &path,
            at(0),
        )
        .unwrap();
        assert_eq!(names(dir.path()), ["config.toml"], "no backup for a no-op");
    }

    #[test]
    fn persist_delete_removes_only_soak_hours_and_file_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL).unwrap();
        apply_persist(PersistAction::Delete, &path, at(0)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.contains("NO_SOAK"), "{text}");
        std::fs::write(&path, "SOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(!path.exists(), "file with no keys left must be deleted");
        std::fs::write(&path, "# only a comment\nSOAK_HOURS = 48\n").unwrap();
        apply_persist(PersistAction::Delete, &path, at(2)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# only a comment\n",
            "a file holding just comments is kept"
        );
    }

    #[test]
    fn persist_leaves_invalid_toml_alone_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[[").unwrap();
        let warn = apply_persist(
            PersistAction::Write(SoakHours::new(12).unwrap()),
            &path,
            at(0),
        )
        .unwrap();
        assert!(warn.is_some_and(|w| w.contains("not valid TOML")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[[");
        let warn = apply_persist(PersistAction::Delete, &path, at(1)).unwrap();
        assert!(warn.is_some());
        assert!(path.exists());
        assert_eq!(
            names(dir.path()),
            ["config.toml"],
            "no backup of a file we did not write"
        );
    }
}
