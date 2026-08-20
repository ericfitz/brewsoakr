use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaIdentity {
    pub version: String,
    pub revision: u32,
    /// Bottle `rebuild` when the `.rb` has a bottle block. Homebrew Cellar
    /// receipts omit that block, so this is `None` and must not be treated as 0.
    pub rebuild: Option<u32>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskIdentity {
    pub version: String,
    pub sha256: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgIdentity {
    Formula(FormulaIdentity),
    Cask(CaskIdentity),
}

impl PkgIdentity {
    /// Installed receipts omit the bottle block. Compare rebuild only when both
    /// sides have one; version, revision, and source sha256 always compare.
    pub fn same_artifact(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Formula(a), Self::Formula(b)) => {
                a.version == b.version
                    && a.revision == b.revision
                    && a.sha256 == b.sha256
                    && match (a.rebuild, b.rebuild) {
                        (Some(x), Some(y)) => x == y,
                        _ => true,
                    }
            }
            (Self::Cask(a), Self::Cask(b)) => a == b,
            _ => false,
        }
    }
}

pub fn parse_formula(rb: &str) -> Result<FormulaIdentity, Error> {
    let git_tag = keyword_quoted(rb, "tag:");
    let git_rev = keyword_quoted(rb, "revision:");
    let version = match first_quoted(rb, "version ") {
        Some(v) => v,
        None => {
            if let Some(tag) = git_tag.as_deref() {
                tag.strip_prefix('v').unwrap_or(tag).to_string()
            } else {
                let url = first_quoted(rb, "url ").ok_or_else(|| {
                    Error::Other("formula missing version, git tag, and url".into())
                })?;
                version_from_url(&url).ok_or_else(|| {
                    Error::Other("could not derive formula version from url".into())
                })?
            }
        }
    };
    let revision = first_u32_in(depth1_lines(rb), "revision ").unwrap_or(0);
    let rebuild = first_u32(rb, "rebuild ");
    let sha256 = first_sha256_before_bottle(rb)
        .or(git_rev)
        .or(git_tag)
        .ok_or_else(|| Error::Other("formula missing sha256 and git revision".into()))?;
    Ok(FormulaIdentity {
        version,
        revision,
        rebuild,
        sha256,
    })
}

/// `tag: "v1.2.3"` / `revision: "abc…"` (Homebrew git-url kwargs).
fn keyword_quoted(rb: &str, key: &str) -> Option<String> {
    for line in rb.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(key) {
            return first_double_quoted(rest.trim_start());
        }
    }
    None
}

pub fn parse_cask(rb: &str) -> Result<CaskIdentity, Error> {
    let version = first_quoted_or_symbol(rb, "version ")
        .ok_or_else(|| Error::Other("cask missing version".into()))?;
    let sha256 = first_quoted_or_symbol(rb, "sha256 ")
        .ok_or_else(|| Error::Other("cask missing sha256".into()))?;
    let url = first_quoted(rb, "url ").ok_or_else(|| Error::Other("cask missing url".into()))?;
    Ok(CaskIdentity {
        version,
        sha256,
        url,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    Deprecated,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleFlag {
    pub kind: LifecycleKind,
    /// ISO `YYYY-MM-DD` from the Homebrew `date:` keyword, if parseable.
    pub date: Option<String>,
}

/// Homebrew `deprecate!` / `disable!` calls. `date:` is a required keyword.
pub fn lifecycle_flags(rb: &str) -> Vec<LifecycleFlag> {
    let mut out = Vec::new();
    for line in rb.lines() {
        let t = line.trim_start();
        let kind = if starts_with_bang_call(t, "deprecate!") {
            LifecycleKind::Deprecated
        } else if starts_with_bang_call(t, "disable!") {
            LifecycleKind::Disabled
        } else {
            continue;
        };
        out.push(LifecycleFlag {
            kind,
            date: date_keyword(t),
        });
    }
    out
}

/// True when a `deprecate!`/`disable!` date is today or earlier, or the date
/// is missing/unparseable. Future dates are not yet in effect (Homebrew
/// `Date.parse(date) <= Date.today`).
pub fn is_deprecated_or_disabled(rb: &str, today: &str) -> bool {
    lifecycle_flags(rb)
        .iter()
        .any(|flag| flag_in_effect(flag, today))
}

pub fn upcoming_lifecycle_messages(rb: &str, today: &str) -> Vec<String> {
    lifecycle_flags(rb)
        .into_iter()
        .filter(|flag| !flag_in_effect(flag, today))
        .filter_map(|flag| {
            let date = flag.date?;
            let word = match flag.kind {
                LifecycleKind::Deprecated => "deprecated",
                LifecycleKind::Disabled => "disabled",
            };
            Some(format!("scheduled to be {word} on {date}"))
        })
        .collect()
}

fn flag_in_effect(flag: &LifecycleFlag, today: &str) -> bool {
    match flag.date.as_deref() {
        Some(d) if is_iso_date(d) => d <= today,
        _ => true,
    }
}

fn date_keyword(line: &str) -> Option<String> {
    let key = "date:";
    let pos = line.find(key)?;
    let rest = line[pos + key.len()..].trim_start();
    first_double_quoted(rest).filter(|d| is_iso_date(d))
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                true
            } else {
                c.is_ascii_digit()
            }
        })
}

fn starts_with_bang_call(line: &str, keyword: &str) -> bool {
    let Some(rest) = line.strip_prefix(keyword) else {
        return false;
    };
    // `\b` after `!`: end of line, or next char is non-word (not alnum/_).
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|c| !c.is_ascii_alphanumeric() && c != '_')
}

fn first_quoted(rb: &str, key: &str) -> Option<String> {
    first_quoted_in(depth1_lines(rb), key)
}

fn first_quoted_in<'a>(lines: impl IntoIterator<Item = &'a str>, key: &str) -> Option<String> {
    for t in lines {
        if let Some(q) = t.strip_prefix(key).and_then(first_double_quoted) {
            return Some(q);
        }
    }
    None
}

/// Lines in the formula/cask body, not inside `resource`/`livecheck`/`bottle`/`on_*`.
/// A body-level `stable do` block is transparent: it holds the stable spec's
/// `url`/`version`/`sha256`, which are the formula's identity. `head do` and
/// every other block stay opaque.
fn depth1_lines(rb: &str) -> Vec<&str> {
    let mut depth = 0i32;
    // One entry per open block; true when the block was transparent.
    let mut open_blocks: Vec<bool> = Vec::new();
    let mut out = Vec::new();
    for line in rb.lines() {
        let t = line.trim_start();
        if is_ruby_end(t) {
            if open_blocks.pop() != Some(true) {
                depth = depth.saturating_sub(1);
            }
            continue;
        }
        let transparent = depth == 1 && is_stable_do(t);
        if depth == 1 && !transparent {
            out.push(t);
        }
        if opens_ruby_block(t) {
            open_blocks.push(transparent);
            if !transparent {
                depth += 1;
            }
        }
    }
    out
}

fn is_stable_do(t: &str) -> bool {
    t.split('#').next().unwrap_or(t).trim_end() == "stable do"
}

fn is_ruby_end(t: &str) -> bool {
    t == "end" || t.starts_with("end ") || t.starts_with("end;") || t.starts_with("end#")
}

fn opens_ruby_block(t: &str) -> bool {
    if t.starts_with("class ") || t.starts_with("module ") || t.starts_with("cask ") {
        return true;
    }
    let stripped = t.split('#').next().unwrap_or(t).trim_end();
    stripped == "do" || stripped.ends_with(" do") || stripped.contains(" do |")
}

/// Quoted string, or a Ruby symbol such as `:latest` / `:no_check`.
fn first_quoted_or_symbol(rb: &str, key: &str) -> Option<String> {
    for t in depth1_lines(rb) {
        let Some(rest) = t.strip_prefix(key) else {
            continue;
        };
        if let Some(q) = first_double_quoted(rest) {
            return Some(q);
        }
        if let Some(sym) = first_ruby_symbol(rest) {
            return Some(sym);
        }
    }
    None
}

fn first_ruby_symbol(s: &str) -> Option<String> {
    let rest = s.trim_start().strip_prefix(':')?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if token.is_empty() {
        None
    } else {
        Some(format!(":{token}"))
    }
}

fn first_double_quoted(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let after = &s[start + 1..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn first_u32(rb: &str, key: &str) -> Option<u32> {
    first_u32_in(rb.lines().map(str::trim_start), key)
}

fn first_u32_in<'a>(lines: impl IntoIterator<Item = &'a str>, key: &str) -> Option<u32> {
    for t in lines {
        if let Some(rest) = t.strip_prefix(key) {
            let token = rest.split_whitespace().next()?;
            if let Ok(n) = token.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

fn is_bottle_do(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("bottle") else {
        return false;
    };
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return false;
    }
    let rest = rest.trim_start();
    let Some(after_do) = rest.strip_prefix("do") else {
        return false;
    };
    after_do.is_empty()
        || after_do
            .chars()
            .next()
            .is_some_and(|c| !c.is_ascii_alphanumeric() && c != '_')
}

fn first_sha256_before_bottle(rb: &str) -> Option<String> {
    for t in depth1_lines(rb) {
        if is_bottle_do(t) {
            break;
        }
        if let Some(q) = t.strip_prefix("sha256 ").and_then(first_double_quoted) {
            return Some(q);
        }
    }
    None
}

fn version_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let segment = path.rsplit('/').next().filter(|s| !s.is_empty())?;
    let base = strip_alpha_extension(strip_archive_suffix(segment));
    Some(version_from_basename(base))
}

/// Homebrew also downloads plain files — `.pem`, `.jar`, `.crate`, a bare
/// `.tar` — whose extension is not part of the version. Drop one trailing
/// `.<ext>` when the extension is all letters, so `wget-1.21.4` keeps its `.4`
/// and `foo-2.0.rc1` keeps its `.rc1`.
fn strip_alpha_extension(name: &str) -> &str {
    let Some((base, ext)) = name.rsplit_once('.') else {
        return name;
    };
    if base.is_empty() || ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphabetic()) {
        return name;
    }
    base
}

fn strip_archive_suffix(name: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lz",
        ".tar.lzma",
        ".tar.Z",
        ".tgz",
        ".tbz",
        ".tbz2",
        ".txz",
        ".zip",
        ".7z",
        ".gz",
        ".bz2",
        ".xz",
        ".zst",
    ];
    for suf in SUFFIXES {
        if let Some(stripped) = name.strip_suffix(suf) {
            return stripped;
        }
        // Case-insensitive for .TAR.GZ etc.
        if name.len() >= suf.len() && name[name.len() - suf.len()..].eq_ignore_ascii_case(suf) {
            return &name[..name.len() - suf.len()];
        }
    }
    name
}

/// `wget-1.21.4` → `1.21.4`: take from the first `-` followed by a digit (or `v`+digit).
fn version_from_basename(base: &str) -> String {
    let bytes = base.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'-' {
            continue;
        }
        let rest = &base[i + 1..];
        if rest.is_empty() {
            continue;
        }
        let b = rest.as_bytes()[0];
        if b.is_ascii_digit() {
            return rest.to_string();
        }
        if b == b'v' && rest.len() > 1 && rest.as_bytes()[1].is_ascii_digit() {
            return rest.to_string();
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_from_url_drops_a_non_archive_extension() {
        // `ca-certificates` ships a bare `.pem`, not a tarball.
        const RB: &str = r#"
class CaCertificates < Formula
  url "https://curl.se/ca/cacert-2026-08-13.pem"
  sha256 "f66dff1bdf8f96060b8177976f8b7d9254bc89bc4db933d769f7384d28480bc9"
end
"#;
        let got = parse_formula(RB).expect("parse ca-certificates");
        assert_eq!(got.version, "2026-08-13");
    }

    #[test]
    fn version_from_url_drops_an_uncompressed_tar() {
        assert_eq!(
            version_from_url("https://example.com/foo-1.2.3.tar"),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn version_from_url_keeps_a_numeric_last_component() {
        assert_eq!(
            version_from_url("https://example.com/wget-1.21.4.tar.gz"),
            Some("1.21.4".to_string())
        );
        assert_eq!(
            version_from_url("https://example.com/foo-2.0.rc1.zip"),
            Some("2.0.rc1".to_string())
        );
    }

    /// `bash`: the stable spec lives in a `stable do` block, so `url`,
    /// `sha256`, and `version` are one level deeper than usual.
    const STABLE_BLOCK_RB: &str = r#"
class Bash < Formula
  desc "Bourne-Again SHell, a UNIX command interpreter"
  head "https://git.savannah.gnu.org/git/bash.git", branch: "master"

  stable do
    url "https://ftpmirror.gnu.org/gnu/bash/bash-5.3.tar.gz"
    mirror "https://ftp.gnu.org/gnu/bash/bash-5.3.tar.gz"
    sha256 "0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba"
    version "5.3.15"
  end

  revision 1

  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_tahoe: "bbb222"
  end
end
"#;

    #[test]
    fn stable_block_supplies_identity() {
        let got = parse_formula(STABLE_BLOCK_RB).expect("parse stable block");
        assert_eq!(
            got,
            FormulaIdentity {
                version: "5.3.15".into(),
                revision: 1,
                rebuild: Some(2),
                sha256: "0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba".into(),
            }
        );
    }

    #[test]
    fn head_block_does_not_supply_identity() {
        const RB: &str = r#"
class Foo < Formula
  head do
    url "https://example.com/foo.git", branch: "main"
    sha256 "headsha"
  end

  stable do
    url "https://example.com/foo-1.0.tar.gz"
    sha256 "aaa111"
  end
end
"#;
        let got = parse_formula(RB).expect("parse head + stable");
        assert_eq!(got.version, "1.0");
        assert_eq!(got.sha256, "aaa111");
    }

    #[test]
    fn resource_inside_stable_block_is_ignored() {
        const RB: &str = r#"
class Foo < Formula
  stable do
    resource "vendored" do
      url "https://example.com/vendored-9.9.tar.gz"
      sha256 "resourcesha"
    end

    url "https://example.com/foo-1.0.tar.gz"
    sha256 "aaa111"
  end
end
"#;
        let got = parse_formula(RB).expect("parse nested resource");
        assert_eq!(got.version, "1.0");
        assert_eq!(got.sha256, "aaa111");
    }

    const WGET_FORMULA: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
  revision 1
  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
"#;

    const WGET_FORMULA_OTHER_SHA: &str = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "zzz999"
  revision 1
  bottle do
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "bbb222"
  end
end
"#;

    const FOO_CASK: &str = r#"
cask "foo" do
  version "3.0"
  sha256 "ccc333"
  url "https://example.com/foo-3.0.dmg"
end
"#;

    const DEPRECATED_FORMULA: &str = r#"
class Old < Formula
  url "https://example.com/old-1.0.tar.gz"
  sha256 "ddd444"
  deprecate! date: "2024-01-01", because: :unmaintained
end
"#;

    #[test]
    fn resource_url_does_not_steal_stable_version() {
        let rb = r#"
class Wget < Formula
  resource "extra" do
    url "https://example.com/extra-9.9.9.tar.gz"
    sha256 "deadbeef"
  end
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
end
"#;
        let id = parse_formula(rb).unwrap();
        assert_eq!(id.version, "1.21.4");
        assert_eq!(id.sha256, "aaa111");
    }

    #[test]
    fn parse_formula_from_url_version_and_source_sha() {
        let id = parse_formula(WGET_FORMULA).unwrap();
        assert_eq!(id.version, "1.21.4");
        assert_eq!(id.revision, 1);
        assert_eq!(id.rebuild, Some(2));
        assert_eq!(id.sha256, "aaa111");
    }

    #[test]
    fn different_source_sha256_yields_different_identity() {
        let a = parse_formula(WGET_FORMULA).unwrap();
        let b = parse_formula(WGET_FORMULA_OTHER_SHA).unwrap();
        assert_eq!(a.version, b.version);
        assert_ne!(a, b);
        assert_ne!(PkgIdentity::Formula(a), PkgIdentity::Formula(b));
    }

    #[test]
    fn parse_cask_fields() {
        let id = parse_cask(FOO_CASK).unwrap();
        assert_eq!(id.version, "3.0");
        assert_eq!(id.sha256, "ccc333");
        assert_eq!(id.url, "https://example.com/foo-3.0.dmg");
    }

    #[test]
    fn receipt_without_bottle_has_no_rebuild() {
        let rb = r#"
class Wget < Formula
  url "https://example.com/wget-1.21.4.tar.gz"
  sha256 "aaa111"
  revision 1
end
"#;
        let id = parse_formula(rb).unwrap();
        assert_eq!(id.rebuild, None);
        let with_bottle = parse_formula(WGET_FORMULA).unwrap();
        assert_eq!(with_bottle.rebuild, Some(2));
        assert!(
            PkgIdentity::Formula(id.clone())
                .same_artifact(&PkgIdentity::Formula(with_bottle.clone()))
        );
        let other_rebuild = FormulaIdentity {
            rebuild: Some(3),
            ..with_bottle.clone()
        };
        assert!(
            !PkgIdentity::Formula(with_bottle).same_artifact(&PkgIdentity::Formula(other_rebuild))
        );
    }

    #[test]
    fn parse_formula_git_tag_and_revision() {
        let rb = r#"
class Helm < Formula
  url "https://github.com/helm/helm.git",
      tag:      "v4.2.3",
      revision: "43e8b7feece8beb0fcba47059ec9b522fd929a64"
  bottle do
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "bbb222"
  end
end
"#;
        let id = parse_formula(rb).unwrap();
        assert_eq!(id.version, "4.2.3");
        assert_eq!(id.sha256, "43e8b7feece8beb0fcba47059ec9b522fd929a64");
    }

    #[test]
    fn parse_formula_git_revision_with_explicit_version() {
        let rb = r#"
class X264 < Formula
  url "https://code.videolan.org/videolan/x264.git",
      revision: "b35605ace3ddf7c1a5d67a2eb553f034aef41d55"
  version "r3222"
  bottle do
    sha256 cellar: :any, arm64_sonoma: "bbb222"
  end
end
"#;
        let id = parse_formula(rb).unwrap();
        assert_eq!(id.version, "r3222");
        assert_eq!(id.sha256, "b35605ace3ddf7c1a5d67a2eb553f034aef41d55");
    }

    #[test]
    fn parse_cask_latest_and_no_check_as_identity_tokens() {
        let rb = r#"
cask "nightly" do
  version :latest
  sha256 :no_check
  url "https://example.com/nightly.dmg"
end
"#;
        let id = parse_cask(rb).unwrap();
        assert_eq!(id.version, ":latest");
        assert_eq!(id.sha256, ":no_check");
        assert_eq!(id.url, "https://example.com/nightly.dmg");
    }

    #[test]
    fn detects_deprecate_bang() {
        assert!(is_deprecated_or_disabled(DEPRECATED_FORMULA, "2024-01-01"));
        assert!(is_deprecated_or_disabled(DEPRECATED_FORMULA, "2026-08-13"));
    }

    #[test]
    fn future_deprecate_date_is_not_yet_in_effect() {
        let rb = r#"
class New < Formula
  url "https://example.com/new-1.0.tar.gz"
  sha256 "ddd444"
  deprecate! date: "2030-11-01", because: :deprecated_upstream
  disable! date: "2031-11-01", because: :deprecated_upstream
end
"#;
        assert!(!is_deprecated_or_disabled(rb, "2026-08-13"));
        let msgs = upcoming_lifecycle_messages(rb, "2026-08-13");
        assert_eq!(
            msgs,
            [
                "scheduled to be deprecated on 2030-11-01",
                "scheduled to be disabled on 2031-11-01",
            ]
        );
    }

    #[test]
    fn missing_date_is_treated_as_in_effect() {
        let rb = "  deprecate! because: :unmaintained\n";
        assert!(is_deprecated_or_disabled(rb, "2026-08-13"));
        assert!(upcoming_lifecycle_messages(rb, "2026-08-13").is_empty());
    }

    #[test]
    fn comment_disable_does_not_match() {
        let rb = "# disable! maybe\nclass X < Formula\nend\n";
        assert!(!is_deprecated_or_disabled(rb, "2026-08-13"));
    }
}
