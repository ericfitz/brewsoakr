use crate::Error;
use crate::git::GitStore;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgKind {
    Formula,
    Cask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgRef {
    pub name: String,
    pub kind: PkgKind,
}

pub fn git_path(pkg: &PkgRef) -> String {
    let root = match pkg.kind {
        PkgKind::Formula => "Formula",
        PkgKind::Cask => "Casks",
    };
    format!(
        "{root}/{}/{}.rb",
        subdirectory(pkg.kind, &pkg.name),
        pkg.name
    )
}

/// Homebrew core/cask tap subdirectory.
/// Formulae whose names start with `lib` live in `Formula/lib/`.
/// Casks whose tokens start with `font-` live in `Casks/font/font-<next-char>/`.
fn subdirectory(kind: PkgKind, name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match kind {
        PkgKind::Formula if lower.starts_with("lib") => "lib".into(),
        PkgKind::Cask if lower.starts_with("font-") => {
            let rest = &lower["font-".len()..];
            let ch = rest.chars().next().unwrap_or('f');
            format!("font/font-{ch}")
        }
        _ => match name.chars().next() {
            Some(c) if c.is_ascii() => c.to_ascii_lowercase().to_string(),
            Some(c) => c.to_string(),
            None => String::new(),
        },
    }
}

pub fn is_third_party(token: &str) -> bool {
    // Two slashes: user/tap/name. One slash: tap/name, not an @ version pin.
    token.contains('/')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlobs {
    pub cutoff: Option<Vec<u8>>,
    pub head: Option<Vec<u8>>,
}

pub fn resolve_blobs(
    git: &impl GitStore,
    repo_dir: &Path,
    cutoff_sha: &str,
    head_sha: &str,
    pkg: &PkgRef,
) -> Result<ResolvedBlobs, Error> {
    let primary = git_path(pkg);
    let mut cutoff = git.show(repo_dir, cutoff_sha, &primary)?;
    let mut head = git.show(repo_dir, head_sha, &primary)?;

    if head.is_none()
        && let Some(canonical) = head_alias_canonical(git, repo_dir, head_sha, &pkg.name)?
    {
        let aliased = git_path(&PkgRef {
            name: canonical,
            kind: pkg.kind,
        });
        head = git.show(repo_dir, head_sha, &aliased)?;
        if cutoff.is_none() {
            cutoff = git.show(repo_dir, cutoff_sha, &aliased)?;
        }
    }

    Ok(ResolvedBlobs { cutoff, head })
}

/// `Aliases/<name>` at HEAD is one line: the canonical formula/cask name.
fn head_alias_canonical(
    git: &impl GitStore,
    repo_dir: &Path,
    head_sha: &str,
    name: &str,
) -> Result<Option<String>, Error> {
    let path = format!("Aliases/{name}");
    let Some(bytes) = git.show(repo_dir, head_sha, &path)? else {
        return Ok(None);
    };
    let canonical = String::from_utf8_lossy(&bytes)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if canonical.is_empty() {
        Ok(None)
    } else {
        Ok(Some(canonical))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::InMemoryGit;
    use std::path::Path;

    fn unused_dir() -> &'static Path {
        Path::new("/brewsoak-in-memory-unused")
    }

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
    fn git_path_wget() {
        assert_eq!(git_path(&formula("wget")), "Formula/w/wget.rb");
    }

    #[test]
    fn git_path_firefox() {
        assert_eq!(git_path(&cask("firefox")), "Casks/f/firefox.rb");
    }

    #[test]
    fn git_path_openssl_at_3() {
        assert_eq!(git_path(&formula("openssl@3")), "Formula/o/openssl@3.rb");
    }

    #[test]
    fn git_path_libpng_uses_lib_subdir() {
        assert_eq!(git_path(&formula("libpng")), "Formula/lib/libpng.rb");
    }

    #[test]
    fn git_path_lib_prefix_even_for_name_lib() {
        assert_eq!(git_path(&formula("lib")), "Formula/lib/lib.rb");
    }

    #[test]
    fn git_path_font_cask_uses_font_subdir() {
        assert_eq!(
            git_path(&cask("font-fira-code")),
            "Casks/font/font-f/font-fira-code.rb"
        );
    }

    #[test]
    fn third_party_user_tap_foo() {
        assert!(is_third_party("user/tap/foo"));
    }

    #[test]
    fn first_party_wget() {
        assert!(!is_third_party("wget"));
    }

    #[test]
    fn first_party_openssl_at_3() {
        assert!(!is_third_party("openssl@3"));
    }

    #[test]
    fn third_party_org_repo_formula() {
        assert!(is_third_party("org/repo/formula"));
    }

    #[test]
    fn third_party_one_slash_is_not_a_version_pin() {
        assert!(is_third_party("user/foo"));
    }

    #[test]
    fn resolve_blobs_primary_wget() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", b"cutoff-wget");
        git.insert_blob("headsha", "Formula/w/wget.rb", b"head-wget");
        let got = resolve_blobs(&git, unused_dir(), "cutoffsha", "headsha", &formula("wget"))
            .expect("resolve");
        assert_eq!(got.cutoff.as_deref(), Some(b"cutoff-wget".as_slice()));
        assert_eq!(got.head.as_deref(), Some(b"head-wget".as_slice()));
    }

    #[test]
    fn resolve_blobs_alias_wget_to_wget_extra() {
        let git = InMemoryGit::new();
        git.insert_blob("headsha", "Aliases/wget", b"wget-extra\n");
        git.insert_blob("cutoffsha", "Formula/w/wget-extra.rb", b"cutoff-wget-extra");
        git.insert_blob("headsha", "Formula/w/wget-extra.rb", b"head-wget-extra");
        let got = resolve_blobs(&git, unused_dir(), "cutoffsha", "headsha", &formula("wget"))
            .expect("resolve");
        assert_eq!(got.cutoff.as_deref(), Some(b"cutoff-wget-extra".as_slice()));
        assert_eq!(got.head.as_deref(), Some(b"head-wget-extra".as_slice()));
    }

    #[test]
    fn resolve_blobs_missing_everywhere() {
        let git = InMemoryGit::new();
        let got = resolve_blobs(&git, unused_dir(), "cutoffsha", "headsha", &formula("wget"))
            .expect("resolve");
        assert_eq!(got.cutoff, None);
        assert_eq!(got.head, None);
    }

    #[test]
    fn resolve_blobs_rename_without_alias_is_not_survived() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", b"cutoff-wget");
        git.insert_blob("headsha", "Formula/w/wget-extra.rb", b"head-wget-extra");
        let got = resolve_blobs(&git, unused_dir(), "cutoffsha", "headsha", &formula("wget"))
            .expect("resolve");
        assert_eq!(got.cutoff.as_deref(), Some(b"cutoff-wget".as_slice()));
        assert_eq!(got.head, None);
    }

    #[test]
    fn resolve_blobs_alias_keeps_cutoff_primary() {
        let git = InMemoryGit::new();
        git.insert_blob("cutoffsha", "Formula/w/wget.rb", b"cutoff-wget");
        git.insert_blob("headsha", "Aliases/wget", b"wget-extra\n");
        git.insert_blob("headsha", "Formula/w/wget-extra.rb", b"head-wget-extra");
        let got = resolve_blobs(&git, unused_dir(), "cutoffsha", "headsha", &formula("wget"))
            .expect("resolve");
        assert_eq!(got.cutoff.as_deref(), Some(b"cutoff-wget".as_slice()));
        assert_eq!(got.head.as_deref(), Some(b"head-wget-extra".as_slice()));
    }
}
