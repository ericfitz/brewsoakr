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
}
