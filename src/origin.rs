//! Where a package comes from (`user/repo`), and the `origins.toml` record
//! brewsoak keeps because a staged install leaves the receipt tap empty.

use crate::Error;
use crate::resolve::PkgKind;
use std::collections::BTreeMap;
use std::path::Path;

pub const CORE: &str = "homebrew/core";
pub const CASK: &str = "homebrew/cask";
/// brewsoak's own local tap. A receipt that names it says nothing about origin.
pub const STAGING_TAP: &str = "brewsoakr/soaked";

pub fn default_origin(kind: PkgKind) -> &'static str {
    match kind {
        PkgKind::Formula => CORE,
        PkgKind::Cask => CASK,
    }
}

pub fn is_core_or_cask(origin: &str) -> bool {
    origin.eq_ignore_ascii_case(CORE) || origin.eq_ignore_ascii_case(CASK)
}

pub fn split_tap(origin: &str) -> Option<(&str, &str)> {
    let (user, repo) = origin.split_once('/')?;
    if user.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((user, repo))
}

pub fn record_key(kind: PkgKind, name: &str) -> String {
    let kind = match kind {
        PkgKind::Formula => "formula",
        PkgKind::Cask => "cask",
    };
    format!("{kind}:{}", name.to_ascii_lowercase())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OriginRecords {
    map: BTreeMap<String, String>,
}

impl OriginRecords {
    fn path(cache: &Path) -> std::path::PathBuf {
        cache.join("origins.toml")
    }

    /// A missing, unreadable, or corrupt file is an empty record set: the
    /// package then falls back to core/cask (spec "Package origin", step 3).
    pub fn load(cache: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(Self::path(cache)) else {
            return Self::default();
        };
        let Ok(table) = toml::from_str::<toml::Table>(&raw) else {
            return Self::default();
        };
        let map = table
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_ascii_lowercase())))
            .collect();
        Self { map }
    }

    pub fn save(&self, cache: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(cache)?;
        let mut body = String::new();
        for (k, v) in &self.map {
            body.push_str(&format!("{k:?} = {v:?}\n"));
        }
        std::fs::write(Self::path(cache), body)?;
        Ok(())
    }

    pub fn get(&self, kind: PkgKind, name: &str) -> Option<&str> {
        self.map.get(&record_key(kind, name)).map(String::as_str)
    }

    pub fn set(&mut self, kind: PkgKind, name: &str, origin: &str) {
        self.map
            .insert(record_key(kind, name), origin.to_ascii_lowercase());
    }

    pub fn remove(&mut self, kind: PkgKind, name: &str) {
        self.map.remove(&record_key(kind, name));
    }
}

/// Spec order: the receipt tap when non-empty and not the staging tap; else
/// brewsoak's record; else `homebrew/core` / `homebrew/cask`.
pub fn resolve_origin(
    receipt_tap: Option<&str>,
    records: &OriginRecords,
    kind: PkgKind,
    name: &str,
) -> String {
    if let Some(tap) = receipt_tap
        && !tap.is_empty()
        && !tap.eq_ignore_ascii_case(STAGING_TAP)
    {
        return tap.to_ascii_lowercase();
    }
    records
        .get(kind, name)
        .map(str::to_string)
        .unwrap_or_else(|| default_origin(kind).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tap_user_repo() {
        assert_eq!(split_tap("hashicorp/tap"), Some(("hashicorp", "tap")));
        assert_eq!(split_tap("wget"), None);
        assert_eq!(split_tap("a/b/c"), None);
    }

    #[test]
    fn records_round_trip_through_origins_toml() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        r.set(PkgKind::Cask, "foo", "acme/casks");
        r.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("origins.toml")).unwrap();
        assert!(
            text.contains("\"formula:terraform\" = \"hashicorp/tap\""),
            "{text}"
        );
        let loaded = OriginRecords::load(dir.path());
        assert_eq!(
            loaded.get(PkgKind::Formula, "terraform"),
            Some("hashicorp/tap")
        );
        assert_eq!(loaded.get(PkgKind::Cask, "foo"), Some("acme/casks"));
        assert_eq!(
            loaded.get(PkgKind::Formula, "foo"),
            None,
            "kind is part of the key"
        );
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        r.remove(PkgKind::Formula, "terraform");
        assert_eq!(r.get(PkgKind::Formula, "terraform"), None);
    }

    #[test]
    fn load_missing_or_corrupt_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(OriginRecords::load(dir.path()), OriginRecords::default());
        std::fs::write(dir.path().join("origins.toml"), "[[[").unwrap();
        assert_eq!(OriginRecords::load(dir.path()), OriginRecords::default());
    }

    #[test]
    fn resolve_origin_prefers_receipt_then_record_then_default() {
        let mut r = OriginRecords::default();
        r.set(PkgKind::Formula, "terraform", "hashicorp/tap");
        assert_eq!(
            resolve_origin(Some("Acme/Tools"), &r, PkgKind::Formula, "terraform"),
            "acme/tools"
        );
        assert_eq!(
            resolve_origin(None, &r, PkgKind::Formula, "terraform"),
            "hashicorp/tap"
        );
        assert_eq!(
            resolve_origin(Some(""), &r, PkgKind::Formula, "terraform"),
            "hashicorp/tap"
        );
        assert_eq!(
            resolve_origin(Some(STAGING_TAP), &r, PkgKind::Formula, "terraform"),
            "hashicorp/tap"
        );
        assert_eq!(resolve_origin(None, &r, PkgKind::Formula, "wget"), CORE);
        assert_eq!(resolve_origin(None, &r, PkgKind::Cask, "firefox"), CASK);
    }
}
