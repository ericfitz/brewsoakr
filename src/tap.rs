use crate::Error;
use crate::brew::Brew;
use crate::resolve::{PkgKind, PkgRef};
use std::collections::HashSet;
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

/// Core dependency names in toposort order (deps first). The target `name` is not included.
pub fn dep_closure(
    brew: &impl Brew,
    kind: PkgKind,
    name: &str,
    is_core: impl Fn(&str) -> bool,
) -> Result<Vec<String>, Error> {
    let mut walk = ClosureWalk {
        brew,
        kind,
        is_core,
        visiting: HashSet::new(),
        visited: HashSet::new(),
        out: Vec::new(),
    };
    walk.visit(name, false)?;
    Ok(walk.out)
}

struct ClosureWalk<'a, B, F> {
    brew: &'a B,
    kind: PkgKind,
    is_core: F,
    visiting: HashSet<String>,
    visited: HashSet<String>,
    out: Vec<String>,
}

impl<B: Brew, F: Fn(&str) -> bool> ClosureWalk<'_, B, F> {
    fn visit(&mut self, name: &str, include_self: bool) -> Result<(), Error> {
        if self.visiting.contains(name) || self.visited.contains(name) {
            return Ok(());
        }
        self.visiting.insert(name.to_string());
        for dep in self.brew.deps(self.kind, name)? {
            if !(self.is_core)(&dep) {
                continue;
            }
            self.visit(&dep, true)?;
        }
        self.visiting.remove(name);
        self.visited.insert(name.to_string());
        if include_self {
            self.out.push(name.to_string());
        }
        Ok(())
    }
}

pub fn brew_install_args(pkg: &PkgRef, path: &Path, user_flags: &[String]) -> Vec<String> {
    let mut args = vec!["install".to_string()];
    args.push(match pkg.kind {
        PkgKind::Formula => "--formula".into(),
        PkgKind::Cask => "--cask".into(),
    });
    for flag in user_flags {
        if is_brew_subcommand(flag) || flag == "--ignore-dependencies" {
            continue;
        }
        args.push(flag.clone());
    }
    args.push(path.to_string_lossy().into_owned());
    args
}

fn is_brew_subcommand(s: &str) -> bool {
    matches!(
        s,
        "install" | "upgrade" | "reinstall" | "update" | "outdated" | "info"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::MockBrew;
    use std::collections::BTreeMap;

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

    fn mock_with_deps(pairs: &[(&str, &[&str])]) -> MockBrew {
        let mut deps = BTreeMap::new();
        for (name, ds) in pairs {
            deps.insert(
                (*name).to_string(),
                ds.iter().map(|s| (*s).to_string()).collect(),
            );
        }
        MockBrew {
            deps,
            ..MockBrew::new()
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
    fn dep_closure_toposorts_deps_excluding_target() {
        let brew = mock_with_deps(&[("foo", &["bar"]), ("bar", &["baz"])]);
        let got = dep_closure(&brew, PkgKind::Formula, "foo", |_| true).expect("closure");
        assert_eq!(got, vec!["baz", "bar"]);
    }

    #[test]
    fn dep_closure_skips_non_core_linux_headers() {
        let brew = mock_with_deps(&[
            ("foo", &["linux-headers", "bar"]),
            ("bar", &["baz"]),
            ("linux-headers", &["hidden"]),
        ]);
        let got =
            dep_closure(&brew, PkgKind::Formula, "foo", |n| n != "linux-headers").expect("closure");
        assert_eq!(got, vec!["baz", "bar"]);
        assert!(!got.iter().any(|n| n == "linux-headers" || n == "hidden"));
    }

    #[test]
    fn dep_closure_empty_when_brew_deps_empty() {
        let brew = MockBrew::new();
        let got = dep_closure(&brew, PkgKind::Formula, "wget", |_| true).expect("closure");
        assert!(got.is_empty());
    }

    #[test]
    fn dep_closure_skips_cycle_on_visiting_path() {
        let brew = mock_with_deps(&[("foo", &["bar"]), ("bar", &["foo"])]);
        let got = dep_closure(&brew, PkgKind::Formula, "foo", |_| true).expect("closure");
        assert_eq!(got, vec!["bar"]);
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
