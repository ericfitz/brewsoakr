//! The `NO_SOAK` list: packages and taps brewsoak hands straight to brew.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSoakEntry {
    /// `wget`: that formula or cask from any origin.
    Name(String),
    /// `ericfitz/tap`: every package whose origin is that tap.
    Tap(String),
    /// `hashicorp/tap/terraform`: that package from that tap only.
    TapPkg { tap: String, name: String },
}

/// Parse one `NO_SOAK` entry. `Err` carries the reason, worded for a `-v` note.
/// Matching is case-insensitive, so entries are lowercased here.
pub fn parse_entry(raw: &str) -> Result<NoSoakEntry, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("NO_SOAK entry is empty; skipped".to_string());
    }
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!(
            "NO_SOAK entry {raw:?} has an empty path segment; skipped"
        ));
    }
    let lower = raw.to_ascii_lowercase();
    match parts.len() {
        1 => Ok(NoSoakEntry::Name(lower)),
        2 => Ok(NoSoakEntry::Tap(lower)),
        3 => {
            let (tap, name) = lower.rsplit_once('/').expect("three segments");
            Ok(NoSoakEntry::TapPkg {
                tap: tap.to_string(),
                name: name.to_string(),
            })
        }
        _ => Err(format!(
            "NO_SOAK entry {raw:?} has more than two slashes; skipped"
        )),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoSoakList {
    entries: Vec<NoSoakEntry>,
}

impl NoSoakList {
    pub fn new(entries: Vec<NoSoakEntry>) -> Self {
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `origin` is `user/repo` (core and cask included); `name` is the formula
    /// name or cask token. Both compare case-insensitively.
    pub fn matches(&self, origin: &str, name: &str) -> bool {
        let origin = origin.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        self.entries.iter().any(|e| match e {
            NoSoakEntry::Name(n) => *n == name,
            NoSoakEntry::Tap(t) => *t == origin,
            NoSoakEntry::TapPkg { tap, name: n } => *tap == origin && *n == name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_matches_any_origin() {
        let list = NoSoakList::new(vec![parse_entry("wget").unwrap()]);
        assert!(list.matches("homebrew/core", "wget"));
        assert!(list.matches("acme/tools", "WGET"));
        assert!(!list.matches("homebrew/core", "wget2"));
    }

    #[test]
    fn one_slash_is_a_whole_tap() {
        let list = NoSoakList::new(vec![parse_entry("EricFitz/Tap").unwrap()]);
        assert!(list.matches("ericfitz/tap", "brewsoak"));
        assert!(list.matches("ericfitz/tap", "anything"));
        assert!(!list.matches("homebrew/core", "brewsoak"));
    }

    #[test]
    fn two_slashes_is_one_package_from_one_tap() {
        let list = NoSoakList::new(vec![parse_entry("hashicorp/tap/terraform").unwrap()]);
        assert!(list.matches("hashicorp/tap", "terraform"));
        assert!(!list.matches("hashicorp/tap", "vault"));
        assert!(!list.matches("homebrew/core", "terraform"));
    }

    #[test]
    fn core_and_cask_are_valid_tap_entries() {
        let list = NoSoakList::new(vec![parse_entry("homebrew/cask").unwrap()]);
        assert!(list.matches("homebrew/cask", "firefox"));
        assert!(!list.matches("homebrew/core", "firefox"));
    }

    #[test]
    fn malformed_entries_are_rejected_with_a_reason() {
        for raw in ["", "  ", "a/b/c/d", "/tap", "tap/", "a//c", "a/b/"] {
            let err = parse_entry(raw).expect_err(raw);
            assert!(
                err.contains(&format!("{raw:?}")) || raw.trim().is_empty(),
                "{raw:?}: {err}"
            );
        }
    }

    #[test]
    fn empty_list_matches_nothing() {
        assert!(NoSoakList::default().is_empty());
        assert!(!NoSoakList::default().matches("homebrew/core", "wget"));
    }
}
