//! Writing staged `.rb` files and building `brew install` args.

use crate::Error;
use crate::resolve::{PkgKind, PkgRef};
use std::path::{Path, PathBuf};

pub fn tap_formula_path(tap_root: &Path, name: &str) -> PathBuf {
    tap_root.join("Formula").join(format!("{name}.rb"))
}

pub fn tap_cask_path(tap_root: &Path, name: &str) -> PathBuf {
    tap_root.join("Casks").join(format!("{name}.rb"))
}

pub fn write_blob(tap_root: &Path, pkg: &PkgRef, blob: &[u8]) -> Result<PathBuf, Error> {
    let path = match pkg.kind {
        PkgKind::Formula => tap_formula_path(tap_root, &pkg.name),
        PkgKind::Cask => tap_cask_path(tap_root, &pkg.name),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = String::from_utf8_lossy(blob);
    std::fs::write(&path, sanitize_unofficial(&text))?;
    Ok(path)
}

/// Drop stanzas that Homebrew only allows in official taps (load-time errors).
pub fn sanitize_unofficial(rb: &str) -> String {
    let mut out = String::with_capacity(rb.len());
    for line in rb.lines() {
        if line.trim_start().starts_with("no_autobump!") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub fn brew_install_args(pkg: &PkgRef, path: &Path, user_flags: &[String]) -> Vec<String> {
    let mut args = vec!["install".to_string()];
    args.push(match pkg.kind {
        PkgKind::Formula => "--formula".into(),
        PkgKind::Cask => "--cask".into(),
    });
    for flag in user_flags {
        if is_stripped_flag(flag) {
            continue;
        }
        args.push(flag.clone());
    }
    args.push(path.to_string_lossy().into_owned());
    args
}

/// Flags brewsoak never forwards to brew: subcommand words and the
/// unsupported `--ignore-dependencies` developer option.
pub(crate) fn is_stripped_flag(s: &str) -> bool {
    is_brew_subcommand(s) || s == "--ignore-dependencies"
}

pub(crate) fn is_brew_subcommand(s: &str) -> bool {
    matches!(
        s,
        "install" | "upgrade" | "reinstall" | "update" | "outdated" | "info"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formula(name: &str) -> PkgRef {
        PkgRef {
            name: name.to_string(),
            kind: PkgKind::Formula,
        }
    }

    fn cask(name: &str) -> PkgRef {
        PkgRef {
            name: name.to_string(),
            kind: PkgKind::Cask,
        }
    }

    #[test]
    fn write_blob_creates_formula_wget_rb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blob = b"class Wget < Formula; end\n";
        let path = write_blob(tmp.path(), &formula("wget"), blob).expect("write");
        assert_eq!(path, tmp.path().join("Formula/wget.rb"));
        assert_eq!(std::fs::read(&path).expect("read"), blob);
    }

    #[test]
    fn write_blob_creates_cask_firefox_rb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blob = b"cask \"firefox\"\n";
        let path = write_blob(tmp.path(), &cask("firefox"), blob).expect("write");
        assert_eq!(path, tmp.path().join("Casks/firefox.rb"));
        assert_eq!(std::fs::read(&path).expect("read"), blob);
    }

    #[test]
    fn brew_install_args_formula_path_install_omits_ignore_deps() {
        let path = Path::new("/tmp/staging/Formula/wget.rb");
        let args = brew_install_args(&formula("wget"), path, &[]);
        assert!(
            !args.iter().any(|a| a == "--ignore-dependencies"),
            "Homebrew treats --ignore-dependencies as an unsupported developer option: {args:?}"
        );
        assert!(args.iter().any(|a| a == path.to_str().unwrap()), "{args:?}");
        assert_eq!(args[0], "install");
        assert!(args.iter().any(|a| a == "--formula"), "{args:?}");
        assert!(
            !args.iter().any(|a| a.contains("brewsoakr/soaked")),
            "path install must not use a tap token: {args:?}"
        );
    }

    #[test]
    fn brew_install_args_strips_user_ignore_dependencies() {
        let path = Path::new("/tmp/staging/Formula/wget.rb");
        let flags = ["--ignore-dependencies".to_string(), "--verbose".to_string()];
        let args = brew_install_args(&formula("wget"), path, &flags);
        assert!(
            !args.iter().any(|a| a == "--ignore-dependencies"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "--verbose"), "{args:?}");
    }

    #[test]
    fn brew_install_args_cask_forwards_user_flags_without_subcommand() {
        let path = Path::new("/tmp/staging/Casks/firefox.rb");
        let flags = ["install".to_string(), "--appdir=/Apps".to_string()];
        let args = brew_install_args(&cask("firefox"), path, &flags);
        assert_eq!(
            args,
            vec![
                "install",
                "--cask",
                "--appdir=/Apps",
                "/tmp/staging/Casks/firefox.rb",
            ]
        );
    }

    #[test]
    fn sanitize_unofficial_strips_no_autobump() {
        let rb = "class Sqlite < Formula\n  url \"https://example.com/s.tgz\"\n  sha256 \"abc\"\n  no_autobump! because: :bumped_by_upstream\nend\n";
        let got = sanitize_unofficial(rb);
        assert!(!got.contains("no_autobump!"), "{got}");
        assert!(got.contains("class Sqlite"), "{got}");
    }
}
