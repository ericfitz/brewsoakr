//! Third-party tap discovery and classification, plus path lookup: what brew
//! has tapped, which taps brewsoak can soak, where their clones and staged
//! files live, and how a name maps to a file in a tap.

use crate::Error;
use crate::brew::{find_json_key, json_string_value, scan_array_objects};
use crate::origin::{self, CASK, CORE, SOAK_TAP, STAGING_TAP};
use crate::resolve::PkgKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapInfo {
    /// `user/repo`, lowercased.
    pub name: String,
    /// `null` for local taps and for API-mode core/cask.
    pub remote: Option<String>,
    /// `path` from tap-info; `None` when absent or empty.
    pub path: Option<PathBuf>,
    /// `trusted` from tap-info: absent is `true`, `true` is `true`, anything else is `false`.
    pub trusted: bool,
}

impl TapInfo {
    pub fn new(name: &str, remote: Option<&str>) -> Self {
        Self {
            name: name.to_ascii_lowercase(),
            remote: remote.map(str::to_string),
            path: None,
            trusted: true,
        }
    }
}

/// `trusted` is `true` when the key is absent; any value other than the
/// literal `true` (`false`, `null`, a string, a number) is `false`.
fn tap_trusted(obj: &str) -> bool {
    let Some(after) = find_json_key(obj, "trusted") else {
        return true;
    };
    let token = after
        .trim_start()
        .split(|c: char| c == ',' || c == '}' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    token == "true"
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
            path: json_string_value(obj, "path")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            trusted: tap_trusted(obj),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapClass {
    Core,
    Cask,
    Staging,
    /// brewsoak's own tap, `ericfitz/brewsoak`: local, so it has no remote.
    SoakTap,
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
    if tap.name == SOAK_TAP {
        return TapClass::SoakTap;
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

/// Every tap named by Homebrew's refusal to load a formula or cask from a tap
/// the user has not trusted (`Refusing to load formula x from untrusted tap
/// u/r.`), lowercase, in the order brew printed them. brewsoak never runs
/// `brew trust` for a third-party tap; that is the user's decision.
pub fn untrusted_taps(brew_output: &str) -> Vec<String> {
    let marker = "from untrusted tap ";
    let lower = brew_output.to_ascii_lowercase();
    lower
        .match_indices(marker)
        .filter_map(|(at, _)| {
            let tap = lower[at + marker.len()..]
                .split_whitespace()
                .next()?
                .trim_end_matches(['.', ',', ';']);
            (!tap.is_empty()).then(|| tap.to_string())
        })
        .collect()
}

/// The tap that blocks a command: brewsoak's own tap when any refusal names
/// it, wherever it falls among several, else the first tap named.
pub fn blocking_untrusted_tap(brew_output: &str) -> Option<String> {
    let all = untrusted_taps(brew_output);
    all.iter()
        .find(|t| t.eq_ignore_ascii_case(SOAK_TAP))
        .or_else(|| all.first())
        .cloned()
}

/// True when brew's output is its `TapFormulaAmbiguityError`: a bare formula
/// name that matches formulae in more than one installed tap.
pub fn ambiguous_formula(brew_output: &str) -> bool {
    brew_output.contains("Formulae found in multiple taps")
        || brew_output.contains("TapFormulaAmbiguityError")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blocking_untrusted_tap_prefers_the_soak_tap_wherever_it_falls() {
        let both = "Refusing to load formula a/b/x from untrusted tap hashicorp/tap.\nRefusing to load formula ericfitz/brewsoak/y from untrusted tap Ericfitz/Brewsoak.";
        assert_eq!(
            blocking_untrusted_tap(both).as_deref(),
            Some("ericfitz/brewsoak")
        );
        let other = "Refusing to load formula a/b/x from untrusted tap hashicorp/tap.";
        assert_eq!(
            blocking_untrusted_tap(other).as_deref(),
            Some("hashicorp/tap")
        );
        assert_eq!(blocking_untrusted_tap("Error: nope"), None);
    }

    #[test]
    fn ambiguous_formula_reads_brews_ambiguity_error() {
        let msg = "Error: Formulae found in multiple taps:\n       * a/tap/cats\n       * b/tap/cats\n\nPlease use the fully-qualified name (e.g. a/tap/cats) to refer to a specific formula.";
        assert!(ambiguous_formula(msg));
        assert!(ambiguous_formula("TapFormulaAmbiguityError: cats"));
        assert!(!ambiguous_formula(
            "Error: No available formula with the name \"cats\"."
        ));
    }

    #[test]
    fn untrusted_taps_read_the_tap_from_brews_refusal() {
        let msg = "Error: packer: Refusing to load formula hashicorp/tap/packer from untrusted tap HashiCorp/tap.\nRun `brew trust hashicorp/tap` to trust it.";
        assert_eq!(untrusted_taps(msg), ["hashicorp/tap"]);
        let cask =
            "Error: Refusing to load cask hashicorp/tap/packer from untrusted tap hashicorp/tap.";
        assert_eq!(untrusted_taps(cask), ["hashicorp/tap"]);
        assert!(untrusted_taps("Error: No bottle available for foo").is_empty());
        assert!(untrusted_taps("Error: cannot load such file -- x").is_empty());
    }

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
        let tap = TapInfo::new("a/b", Some("http://example.com/a/homebrew-b"));
        assert_eq!(classify(&tap), TapClass::Unsoakable);
    }

    #[test]
    fn parse_tap_info_reads_path_and_trusted() {
        let json = r#"[
          {"name": "a/t1", "trusted": true, "path": "/x/Taps/ericfitz/homebrew-brewsoak"},
          {"name": "a/t2", "trusted": false},
          {"name": "a/t3"},
          {"name": "a/t4", "trusted": "yes"},
          {"name": "a/t5", "trusted": 1},
          {"name": "a/t6", "trusted": null, "path": ""}
        ]"#;
        let taps = parse_tap_info_json(json).unwrap();
        let trusted: Vec<bool> = taps.iter().map(|t| t.trusted).collect();
        assert_eq!(trusted, [true, false, true, false, false, false]);
        assert_eq!(
            taps[0].path,
            Some(PathBuf::from("/x/Taps/ericfitz/homebrew-brewsoak"))
        );
        assert_eq!(taps[1].path, None);
        assert_eq!(taps[5].path, None, "an empty path is no path");
    }

    #[test]
    fn classify_names_the_soak_tap_before_the_remote_rule() {
        assert_eq!(
            classify(&TapInfo::new("ericfitz/brewsoak", None)),
            TapClass::SoakTap
        );
        assert_eq!(
            classify(&TapInfo::new(
                "ericfitz/brewsoak",
                Some("https://github.com/ericfitz/homebrew-brewsoak")
            )),
            TapClass::SoakTap
        );
        assert_eq!(
            classify(&TapInfo::new("brewsoakr/soaked", None)),
            TapClass::Staging
        );
    }

    #[test]
    fn cloned_ericfitz_tap_stays_soakable() {
        let tap = TapInfo::new(
            "ericfitz/tap",
            Some("https://github.com/ericfitz/homebrew-tap"),
        );
        assert_eq!(classify(&tap), TapClass::Soakable);
    }

    #[test]
    fn clone_dirs_are_per_tap() {
        assert_eq!(
            clone_dir(Path::new("/cache"), "hashicorp/tap"),
            PathBuf::from("/cache/taps/hashicorp/tap.git")
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
}
