use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaIdentity {
    pub version: String,
    pub revision: u32,
    pub rebuild: u32,
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

pub fn parse_formula(rb: &str) -> Result<FormulaIdentity, Error> {
    let version = match first_quoted(rb, "version ") {
        Some(v) => v,
        None => {
            let url = first_quoted(rb, "url ")
                .ok_or_else(|| Error::Other("formula missing version and url".into()))?;
            version_from_url(&url)
                .ok_or_else(|| Error::Other("could not derive formula version from url".into()))?
        }
    };
    let revision = first_u32(rb, "revision ").unwrap_or(0);
    let rebuild = first_u32(rb, "rebuild ").unwrap_or(0);
    let sha256 = first_sha256_before_bottle(rb)
        .or_else(|| first_quoted(rb, "sha256 "))
        .ok_or_else(|| Error::Other("formula missing sha256".into()))?;
    Ok(FormulaIdentity {
        version,
        revision,
        rebuild,
        sha256,
    })
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

/// True if any line matches `^\s*(deprecate!|disable!)\b`.
pub fn is_deprecated_or_disabled(rb: &str) -> bool {
    rb.lines().any(|line| {
        let t = line.trim_start();
        starts_with_bang_call(t, "deprecate!") || starts_with_bang_call(t, "disable!")
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
    for line in rb.lines() {
        let t = line.trim_start();
        if let Some(q) = t.strip_prefix(key).and_then(first_double_quoted) {
            return Some(q);
        }
    }
    None
}

/// Quoted string, or a Ruby symbol such as `:latest` / `:no_check`.
fn first_quoted_or_symbol(rb: &str, key: &str) -> Option<String> {
    for line in rb.lines() {
        let t = line.trim_start();
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
    for line in rb.lines() {
        let t = line.trim_start();
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
    for line in rb.lines() {
        if is_bottle_do(line) {
            break;
        }
        let t = line.trim_start();
        if let Some(q) = t.strip_prefix("sha256 ").and_then(first_double_quoted) {
            return Some(q);
        }
    }
    None
}

fn version_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let segment = path.rsplit('/').next().filter(|s| !s.is_empty())?;
    let base = strip_archive_suffix(segment);
    Some(version_from_basename(base))
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
    fn parse_formula_from_url_version_and_source_sha() {
        let id = parse_formula(WGET_FORMULA).unwrap();
        assert_eq!(id.version, "1.21.4");
        assert_eq!(id.revision, 1);
        assert_eq!(id.rebuild, 2);
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
        assert!(is_deprecated_or_disabled(DEPRECATED_FORMULA));
    }

    #[test]
    fn comment_disable_does_not_match() {
        let rb = "# disable! maybe\nclass X < Formula\nend\n";
        assert!(!is_deprecated_or_disabled(rb));
    }
}
