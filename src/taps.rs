//! Third-party tap discovery and classification, plus path lookup: what brew
//! has tapped, which taps brewsoak can soak, where their clones and staged
//! files live, and how a name maps to a file in a tap.

use crate::Error;
use crate::brew::{json_string_value, scan_array_objects};
use crate::origin::{self, CASK, CORE, STAGING_TAP};
use crate::resolve::PkgKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapInfo {
    /// `user/repo`, lowercased.
    pub name: String,
    /// `null` for local taps and for API-mode core/cask.
    pub remote: Option<String>,
}

/// `brew tap-info --json --installed` prints a top-level array of tap objects.
pub fn parse_tap_info_json(json: &str) -> Result<Vec<TapInfo>, Error> {
    let after = json.trim_start();
    let Some(rest) = after.strip_prefix('[') else {
        return Err(Error::Other("tap-info json is not an array".into()));
    };
    let objects = scan_array_objects(rest)
        .ok_or_else(|| Error::Other("tap-info json array is malformed".into()))?;
    let mut out = Vec::new();
    for obj in objects {
        let Some(name) = json_string_value(obj, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        out.push(TapInfo {
            name: name.to_ascii_lowercase(),
            remote: json_string_value(obj, "remote").filter(|r| !r.is_empty()),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapClass {
    Core,
    Cask,
    Staging,
    /// No remote, or not `https://`: brewsoak never installs from it.
    Unsoakable,
    Soakable,
}

/// Name matches come first: API-mode `homebrew/core` and `homebrew/cask`
/// report `remote: null` too (spec issue 2).
pub fn classify(tap: &TapInfo) -> TapClass {
    if tap.name == CORE {
        return TapClass::Core;
    }
    if tap.name == CASK {
        return TapClass::Cask;
    }
    if tap.name == STAGING_TAP {
        return TapClass::Staging;
    }
    match tap.remote.as_deref() {
        Some(r) if r.starts_with("https://") => TapClass::Soakable,
        _ => TapClass::Unsoakable,
    }
}

/// Bare clone for a tap's soak history. Never a repo under brew's `Library/Taps`.
pub fn clone_dir(cache: &Path, tap: &str) -> PathBuf {
    let (user, repo) = origin::split_tap(tap).unwrap_or((tap, "tap"));
    cache.join("taps").join(user).join(format!("{repo}.git"))
}

/// Per-tap staging directory so a tap formula never collides with a core one.
/// Core and cask keep the v1 layout directly under `tap_root`.
pub fn staging_root(tap_root: &Path, origin_tap: &str) -> PathBuf {
    if origin::is_core_or_cask(origin_tap) {
        return tap_root.to_path_buf();
    }
    let (user, repo) = origin::split_tap(origin_tap).unwrap_or((origin_tap, "tap"));
    tap_root.join("taps").join(user).join(repo)
}

/// Homebrew's lookup order for a tap. Tap aliases and renames are not followed.
pub fn resolve_path(tree: &[String], kind: PkgKind, name: &str) -> Option<String> {
    let file = format!("{name}.rb");
    let has = |p: &str| tree.iter().any(|t| t == p).then(|| p.to_string());
    let sharded = |dir: &str| {
        let prefix = format!("{dir}/");
        let suffix = format!("/{file}");
        tree.iter()
            .find(|t| t.starts_with(&prefix) && t.ends_with(&suffix))
            .cloned()
    };
    match kind {
        PkgKind::Formula => has(&format!("Formula/{file}"))
            .or_else(|| sharded("Formula"))
            .or_else(|| has(&format!("HomebrewFormula/{file}")))
            .or_else(|| has(&file)),
        PkgKind::Cask => has(&format!("Casks/{file}")).or_else(|| sharded("Casks")),
    }
}

/// brew could not load the staged `.rb` at all (as opposed to failing to
/// build or download it): the formula reaches outside its own file.
pub fn staged_load_failure(brew_output: &str) -> bool {
    let s = brew_output.to_ascii_lowercase();
    s.contains("cannot load such file")
        || s.contains("invalid formula")
        || s.contains("invalid cask")
        || s.contains("uninitialized constant")
        || s.contains("undefined method")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAP_INFO: &str = r#"[
      {"name": "homebrew/core", "remote": null, "path": "/x", "private": false, "formula_names": [], "cask_tokens": []},
      {"name": "homebrew/cask", "remote": null},
      {"name": "brewsoakr/soaked", "remote": null, "path": "/opt/homebrew/Library/Taps/brewsoakr/homebrew-soaked"},
      {"name": "HashiCorp/tap", "remote": "https://github.com/hashicorp/homebrew-tap", "private": false},
      {"name": "acme/private", "remote": "git@github.com:acme/homebrew-private.git"},
      {"name": "local/tap", "remote": null}
    ]"#;

    #[test]
    fn parse_tap_info_reads_name_and_remote() {
        let taps = parse_tap_info_json(TAP_INFO).unwrap();
        assert_eq!(taps.len(), 6);
        assert_eq!(taps[3].name, "hashicorp/tap", "names are lowercased");
        assert_eq!(
            taps[3].remote.as_deref(),
            Some("https://github.com/hashicorp/homebrew-tap")
        );
        assert_eq!(taps[0].remote, None);
    }

    #[test]
    fn classify_follows_the_spec_table() {
        let taps = parse_tap_info_json(TAP_INFO).unwrap();
        let classes: Vec<TapClass> = taps.iter().map(classify).collect();
        assert_eq!(
            classes,
            vec![
                TapClass::Core,
                TapClass::Cask,
                TapClass::Staging,
                TapClass::Soakable,
                TapClass::Unsoakable,
                TapClass::Unsoakable,
            ]
        );
    }

    #[test]
    fn http_remote_is_unsoakable() {
        let tap = TapInfo {
            name: "a/b".into(),
            remote: Some("http://example.com/a/homebrew-b".into()),
        };
        assert_eq!(classify(&tap), TapClass::Unsoakable);
    }

    #[test]
    fn clone_and_staging_dirs_are_per_tap() {
        assert_eq!(
            clone_dir(Path::new("/cache"), "hashicorp/tap"),
            PathBuf::from("/cache/taps/hashicorp/tap.git")
        );
        assert_eq!(
            staging_root(Path::new("/cache/staging"), "hashicorp/tap"),
            PathBuf::from("/cache/staging/taps/hashicorp/tap")
        );
        assert_eq!(
            staging_root(Path::new("/cache/staging"), "homebrew/core"),
            PathBuf::from("/cache/staging")
        );
        assert_eq!(
            staging_root(Path::new("/cache/staging"), "homebrew/cask"),
            PathBuf::from("/cache/staging")
        );
    }

    fn tree(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn resolve_path_formula_search_order() {
        let t = tree(&[
            "Formula/terraform.rb",
            "Formula/t/vault.rb",
            "HomebrewFormula/consul.rb",
            "nomad.rb",
            "README.md",
        ]);
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "terraform").as_deref(),
            Some("Formula/terraform.rb")
        );
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "vault").as_deref(),
            Some("Formula/t/vault.rb")
        );
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "consul").as_deref(),
            Some("HomebrewFormula/consul.rb")
        );
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "nomad").as_deref(),
            Some("nomad.rb")
        );
        assert_eq!(resolve_path(&t, PkgKind::Formula, "readme"), None);
        assert_eq!(resolve_path(&t, PkgKind::Formula, "missing"), None);
    }

    #[test]
    fn resolve_path_prefers_flat_formula_over_sharded() {
        let t = tree(&["Formula/x/foo.rb", "Formula/foo.rb"]);
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "foo").as_deref(),
            Some("Formula/foo.rb")
        );
    }

    #[test]
    fn resolve_path_cask_search_order() {
        let t = tree(&["Casks/foo.rb", "Casks/b/bar.rb", "Formula/bar.rb"]);
        assert_eq!(
            resolve_path(&t, PkgKind::Cask, "foo").as_deref(),
            Some("Casks/foo.rb")
        );
        assert_eq!(
            resolve_path(&t, PkgKind::Cask, "bar").as_deref(),
            Some("Casks/b/bar.rb")
        );
        assert_eq!(resolve_path(&t, PkgKind::Cask, "baz"), None);
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "foo"),
            None,
            "casks are not formulae"
        );
    }

    #[test]
    fn resolve_path_does_not_match_version_suffix_collisions() {
        let t = tree(&["Formula/python@3.12.rb", "Formula/python.rb"]);
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "python").as_deref(),
            Some("Formula/python.rb")
        );
        assert_eq!(
            resolve_path(&t, PkgKind::Formula, "python@3.12").as_deref(),
            Some("Formula/python@3.12.rb")
        );
    }

    #[test]
    fn staged_load_failure_matches_ruby_load_errors() {
        assert!(staged_load_failure(
            "Error: cannot load such file -- /x/staging/taps/a/b/Formula/../lib/helper"
        ));
        assert!(staged_load_failure(
            "Error: Invalid formula: /x/Formula/foo.rb\nfoo: uninitialized constant Helper"
        ));
        assert!(staged_load_failure(
            "Error: foo: undefined method `helper' for"
        ));
        assert!(!staged_load_failure("Error: No bottle available for foo"));
        assert!(!staged_load_failure(
            "Warning: foo 1.0 is already installed and up-to-date."
        ));
    }
}
