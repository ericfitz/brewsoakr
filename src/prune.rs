//! Cache pruning after a refresh: clones and staging directories of taps that
//! are no longer tapped, and `origins.toml` records of packages that are no
//! longer installed. Everything removed is regenerable cache. Runs under the
//! run lock. Only paths under `<cache>` are touched; never anything under
//! `$(brew --repository)/Library/Taps`.

use crate::Error;
use crate::inventory::Inventory;
use crate::origin::OriginRecords;
use std::io::Write;
use std::path::Path;

/// Remove what an untapped tap and an uninstalled package left behind. Safe to
/// repeat: a second run finds nothing, and a run that died halfway is finished
/// by the next. A removal that fails is reported and skipped, not fatal.
pub fn prune_cache(
    cache: &Path,
    inv: &Inventory,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    let tapped = |tap: &str| inv.taps.contains_key(&tap.to_ascii_lowercase());
    prune_untapped(&cache.join("taps"), Some(".git"), &tapped, verbose, out)?;
    prune_untapped(&cache.join("staging/taps"), None, &tapped, verbose, out)?;
    let mut records = OriginRecords::load(cache);
    let removed = records.remove_uninstalled(|kind, name| {
        inv.pkgs
            .iter()
            .any(|p| p.kind == kind && p.name.eq_ignore_ascii_case(name))
    });
    if removed.is_empty() {
        return Ok(());
    }
    records.save(cache)?;
    if verbose {
        for (key, origin) in removed {
            writeln!(out, "pruned origin record {key} ({origin}): not installed")?;
        }
    }
    Ok(())
}

/// `root/<user>/<repo><suffix>` directories whose `user/repo` is not tapped.
fn prune_untapped(
    root: &Path,
    suffix: Option<&str>,
    tapped: &impl Fn(&str) -> bool,
    verbose: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    for user in subdirs(root) {
        let Some(user_name) = user.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        for repo in subdirs(&user) {
            let Some(entry) = repo.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let repo_name = match suffix {
                Some(suffix) => match entry.strip_suffix(suffix) {
                    Some(name) if !name.is_empty() => name,
                    _ => continue,
                },
                None => entry,
            };
            let tap = format!("{user_name}/{repo_name}");
            if tapped(&tap) {
                continue;
            }
            match std::fs::remove_dir_all(&repo) {
                Ok(()) if verbose => writeln!(
                    out,
                    "pruned {} (tap {} is not tapped)",
                    repo.display(),
                    tap.to_ascii_lowercase()
                )?,
                Ok(()) => {}
                Err(e) => writeln!(
                    out,
                    "brewsoak: warning: could not prune {}: {e}",
                    repo.display()
                )?,
            }
        }
        // An emptied user directory goes too; a non-empty one stays.
        let _ = std::fs::remove_dir(&user);
    }
    Ok(())
}

/// Real subdirectories of `dir` (symlinks are not followed or removed);
/// empty when `dir` does not exist or cannot be read.
fn subdirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Pkg, PkgClass};
    use crate::origin::OriginRecords;
    use crate::resolve::PkgKind;
    use crate::taps::TapClass;

    fn pkg(name: &str, kind: PkgKind, origin: &str) -> Pkg {
        Pkg {
            name: name.into(),
            kind,
            origin: origin.into(),
            class: PkgClass::Unsoakable,
            receipt_rb: String::new(),
            receipt_tap: None,
            pinned: false,
        }
    }

    fn inv(taps: &[&str], pkgs: Vec<Pkg>) -> Inventory {
        Inventory {
            pkgs,
            taps: taps
                .iter()
                .map(|t| ((*t).to_string(), TapClass::Soakable))
                .collect(),
            tap_remotes: Default::default(),
        }
    }

    fn mk(cache: &Path, rel: &str) {
        std::fs::create_dir_all(cache.join(rel)).unwrap();
        std::fs::write(cache.join(rel).join("f"), "x").unwrap();
    }

    fn seed(cache: &Path) {
        for rel in [
            "taps/gone/tap.git",
            "taps/live/tap.git",
            "staging/taps/gone/tap/Formula",
            "staging/taps/live/tap/Formula",
        ] {
            mk(cache, rel);
        }
        let mut records = OriginRecords::default();
        records.set(PkgKind::Formula, "stale", "gone/tap");
        records.set(PkgKind::Formula, "kept", "gone/tap");
        records.set(PkgKind::Cask, "caskkept", "live/tap");
        records.save(cache).unwrap();
    }

    fn world() -> Inventory {
        inv(
            &["live/tap"],
            vec![
                pkg("kept", PkgKind::Formula, "gone/tap"),
                pkg("caskkept", PkgKind::Cask, "live/tap"),
            ],
        )
    }

    #[test]
    fn untapped_tap_dirs_and_uninstalled_records_go_and_installed_records_stay() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        seed(cache);
        let mut out = Vec::new();
        prune_cache(cache, &world(), true, &mut out).unwrap();
        assert!(!cache.join("taps/gone").exists());
        assert!(!cache.join("staging/taps/gone").exists());
        assert!(cache.join("taps/live/tap.git/f").exists());
        assert!(cache.join("staging/taps/live/tap/Formula/f").exists());
        let records = OriginRecords::load(cache);
        assert_eq!(records.get(PkgKind::Formula, "stale"), None);
        assert_eq!(
            records.get(PkgKind::Formula, "kept"),
            Some("gone/tap"),
            "an installed package keeps its record though its tap is gone"
        );
        assert_eq!(records.get(PkgKind::Cask, "caskkept"), Some("live/tap"));
        let text = String::from_utf8(out).unwrap();
        for want in [
            "taps/gone/tap.git",
            "staging/taps/gone/tap",
            "formula:stale",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
        assert!(!text.contains("live/tap"), "{text}");
    }

    #[test]
    fn quiet_run_prints_nothing_and_a_second_run_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        seed(cache);
        let mut out = Vec::new();
        prune_cache(cache, &world(), false, &mut out).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        let records = std::fs::read(cache.join("origins.toml")).unwrap();
        let modified = std::fs::metadata(cache.join("origins.toml"))
            .unwrap()
            .modified()
            .unwrap();
        let mut again = Vec::new();
        prune_cache(cache, &world(), true, &mut again).unwrap();
        assert!(again.is_empty(), "{}", String::from_utf8_lossy(&again));
        assert_eq!(std::fs::read(cache.join("origins.toml")).unwrap(), records);
        assert_eq!(
            std::fs::metadata(cache.join("origins.toml"))
                .unwrap()
                .modified()
                .unwrap(),
            modified,
            "a no-op run does not rewrite origins.toml"
        );
    }

    #[test]
    fn leftovers_of_a_half_pruned_run_are_finished() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        seed(cache);
        // A previous run removed the clone and some of the staging files only.
        std::fs::remove_dir_all(cache.join("taps/gone/tap.git")).unwrap();
        std::fs::remove_file(cache.join("staging/taps/gone/tap/Formula/f")).unwrap();
        prune_cache(cache, &world(), false, &mut Vec::new()).unwrap();
        assert!(!cache.join("taps/gone").exists());
        assert!(!cache.join("staging/taps/gone").exists());
    }

    #[test]
    fn missing_cache_dirs_and_stray_files_are_fine() {
        let dir = tempfile::tempdir().unwrap();
        prune_cache(dir.path(), &world(), true, &mut Vec::new()).unwrap();
        std::fs::create_dir_all(dir.path().join("taps/user")).unwrap();
        std::fs::write(dir.path().join("taps/user/notes.txt"), "x").unwrap();
        std::fs::write(dir.path().join("taps/loose"), "x").unwrap();
        prune_cache(dir.path(), &world(), true, &mut Vec::new()).unwrap();
        assert!(dir.path().join("taps/user/notes.txt").exists());
        assert!(dir.path().join("taps/loose").exists());
    }

    #[test]
    fn tap_names_compare_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        mk(dir.path(), "taps/Live/Tap.git");
        prune_cache(dir.path(), &world(), false, &mut Vec::new()).unwrap();
        assert!(dir.path().join("taps/Live/Tap.git/f").exists());
    }
}
