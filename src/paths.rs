use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_under_home_config() {
        assert!(config_file().ends_with(".config/brewsoak/config.toml"));
    }
}
