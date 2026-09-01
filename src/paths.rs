use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_file() -> PathBuf {
    home_dir().join(".config/brewsoak/config.toml")
}

pub fn cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("brewsoak");
    }
    home_dir().join("Library/Caches/brewsoak")
}

pub fn brew_bin() -> PathBuf {
    brew_bin_from_env(
        std::env::var("HOMEBREW_PREFIX").ok().as_deref(),
        Path::exists,
    )
}

pub fn brew_bin_from_env(prefix: Option<&str>, path_exists: impl Fn(&Path) -> bool) -> PathBuf {
    if let Some(prefix) = prefix {
        return PathBuf::from(prefix).join("bin/brew");
    }
    let default = Path::new("/opt/homebrew/bin/brew");
    if path_exists(default) {
        default.to_path_buf()
    } else {
        PathBuf::from("brew")
    }
}

/// Both a Homebrew-installed and a cargo-installed brewsoak on disk: returns the
/// shadowed one (the copy that is not running) and the command to remove it.
pub fn duplicate_install(
    current_exe: &Path,
    brew_bin: &Path,
    cargo_home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    // Bare "brew" (no prefix found) means we cannot locate the Homebrew copy.
    brew_bin.parent()?.parent()?;
    let brew_copy = brew_bin.with_file_name("brewsoak");
    let cargo_copy = cargo_home.join("bin/brewsoak");
    if !exists(&brew_copy) || !exists(&cargo_copy) {
        return None;
    }
    // current_exe is canonicalized, so the Homebrew symlink resolves into the Cellar.
    if current_exe.starts_with(cargo_home) && !cargo_home.as_os_str().is_empty() {
        Some((brew_copy, "brew uninstall brewsoak"))
    } else {
        Some((cargo_copy, "cargo uninstall brewsoak"))
    }
}

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"))
}

/// Print a warning to stderr when brewsoak is installed via both Homebrew and cargo.
pub fn warn_duplicate_install() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.canonicalize().unwrap_or(exe);
    let Some((shadowed, cmd)) = duplicate_install(&exe, &brew_bin(), &cargo_home(), Path::exists)
    else {
        return;
    };
    eprintln!("brewsoak: warning: installed twice (Homebrew and cargo).");
    eprintln!("  running:   {}", exe.display());
    eprintln!("  shadowed:  {}", shadowed.display());
    eprintln!("  remove it with: {cmd}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_under_home_config() {
        assert!(config_file().ends_with(".config/brewsoak/config.toml"));
    }

    #[test]
    fn brew_bin_honors_homebrew_prefix() {
        let got = brew_bin_from_env(Some("/custom/prefix"), |_| false);
        assert_eq!(got, PathBuf::from("/custom/prefix/bin/brew"));
    }

    #[test]
    fn brew_bin_uses_default_prefix_when_present() {
        let got = brew_bin_from_env(None, |p| p == Path::new("/opt/homebrew/bin/brew"));
        assert_eq!(got, PathBuf::from("/opt/homebrew/bin/brew"));
    }

    #[test]
    fn brew_bin_falls_back_to_path_name() {
        let got = brew_bin_from_env(None, |_| false);
        assert_eq!(got, PathBuf::from("brew"));
    }

    #[test]
    fn duplicate_install_reports_cargo_copy_when_brew_copy_runs() {
        let got = duplicate_install(
            Path::new("/opt/homebrew/Cellar/brewsoak/1.0/bin/brewsoak"),
            Path::new("/opt/homebrew/bin/brew"),
            Path::new("/home/u/.cargo"),
            |_| true,
        );
        assert_eq!(
            got,
            Some((
                PathBuf::from("/home/u/.cargo/bin/brewsoak"),
                "cargo uninstall brewsoak"
            ))
        );
    }

    #[test]
    fn duplicate_install_reports_brew_copy_when_cargo_copy_runs() {
        let got = duplicate_install(
            Path::new("/home/u/.cargo/bin/brewsoak"),
            Path::new("/opt/homebrew/bin/brew"),
            Path::new("/home/u/.cargo"),
            |_| true,
        );
        assert_eq!(
            got,
            Some((
                PathBuf::from("/opt/homebrew/bin/brewsoak"),
                "brew uninstall brewsoak"
            ))
        );
    }

    #[test]
    fn duplicate_install_silent_when_only_one_copy() {
        let got = duplicate_install(
            Path::new("/home/u/.cargo/bin/brewsoak"),
            Path::new("/opt/homebrew/bin/brew"),
            Path::new("/home/u/.cargo"),
            |p| p.starts_with("/home/u/.cargo"),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn duplicate_install_silent_without_brew_prefix() {
        let got = duplicate_install(
            Path::new("/home/u/.cargo/bin/brewsoak"),
            Path::new("brew"),
            Path::new("/home/u/.cargo"),
            |_| true,
        );
        assert_eq!(got, None);
    }
}
