use crate::Error;
use std::cmp::Ordering;

/// What brew said about the machine it runs on: the CPU architecture, the
/// macOS codename, and every codename brew knows with its version number.
/// brewsoak holds no codename table of its own; `brew::HOST_PLATFORM_RUBY`
/// reads all of this from `MacOSVersion::SYMBOLS` and `MacOS.version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPlatform {
    /// `arm64` or `x86_64`, as `Hardware::CPU.arch` prints it.
    pub arch: String,
    /// The running macOS codename, e.g. `sequoia`.
    pub macos: String,
    /// Every `(codename, version)` brew knows, e.g. `("big_sur", "11")`.
    pub macos_versions: Vec<(String, String)>,
}

impl HostPlatform {
    /// Reads the output of `brew::HOST_PLATFORM_RUBY`: one `<codename>
    /// <version>` line per known release, then `HOST <codename> <arch>`.
    /// `None` unless the host line is present, names a known codename and a
    /// known architecture, and every other line has a version.
    pub fn parse(output: &str) -> Option<Self> {
        let mut versions = Vec::new();
        let mut host = None;
        for line in output.lines().filter(|l| !l.trim().is_empty()) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.as_slice() {
                ["HOST", codename, arch] => host = Some((*codename, *arch)),
                [codename, version] if *codename != "HOST" => {
                    parse_dotted(version)?;
                    versions.push((codename.to_string(), version.to_string()));
                }
                _ => return None,
            }
        }
        let (macos, arch) = host?;
        if !matches!(arch, "arm64" | "x86_64") || !versions.iter().any(|(c, _)| c == macos) {
            return None;
        }
        Some(Self {
            arch: arch.to_string(),
            macos: macos.to_string(),
            macos_versions: versions,
        })
    }

    fn is_arm(&self) -> bool {
        self.arch == "arm64"
    }

    /// The `arm:`/`intel:` keyword that applies to this host.
    fn arch_keyword(&self) -> &'static str {
        if self.is_arm() { "arm:" } else { "intel:" }
    }

    fn version_of(&self, codename: &str) -> Option<Vec<u32>> {
        let (_, v) = self.macos_versions.iter().find(|(c, _)| c == codename)?;
        parse_dotted(v)
    }

    /// Whether a cask body line such as `on_arm do` or `on_monterey :or_newer
    /// do` opens a block that applies to this host. Anything unrecognized,
    /// including an unknown codename, does not apply.
    fn block_applies(&self, line: &str) -> bool {
        let code = line.split('#').next().unwrap_or(line).trim();
        let Some(head) = code.strip_suffix(" do") else {
            return false;
        };
        let mut words = head.split_whitespace();
        let (Some(name), qualifier, None) = (words.next(), words.next(), words.next()) else {
            return false;
        };
        match (name, qualifier) {
            ("on_arm", None) => self.is_arm(),
            ("on_intel", None) => !self.is_arm(),
            ("on_macos", None) => true,
            (_, q) => {
                let Some(target) = name.strip_prefix("on_").and_then(|c| self.version_of(c)) else {
                    return false;
                };
                let Some(host) = self.version_of(&self.macos) else {
                    return false;
                };
                match (q, cmp_dotted(&host, &target)) {
                    (None, o) => o == Ordering::Equal,
                    (Some(":or_newer"), o) => o != Ordering::Less,
                    (Some(":or_older"), o) => o != Ordering::Greater,
                    _ => false,
                }
            }
        }
    }
}

fn parse_dotted(v: &str) -> Option<Vec<u32>> {
    v.split('.').map(|p| p.parse().ok()).collect()
}

fn cmp_dotted(a: &[u32], b: &[u32]) -> Ordering {
    (0..a.len().max(b.len()))
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .cmp(&b.get(i).copied().unwrap_or(0))
        })
        .find(|o| *o != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaIdentity {
    pub version: String,
    pub revision: u32,
    /// Bottle `rebuild` when the `.rb` has a bottle block. Homebrew Cellar
    /// receipts omit that block, so this is `None` and must not be treated as 0.
    pub rebuild: Option<u32>,
    pub sha256: String,
    /// What `version` was derived from, when no `version` stanza gave it:
    /// `version` is then only a local guess, and brew's `Version.detect` of
    /// this source is the better answer. `None` for an explicit `version`.
    /// Not part of `same_artifact`.
    pub derived_from: Option<VersionSource>,
}

/// The `url` (and git `tag:`) a formula version was derived from, as
/// `Version.detect(url, tag:)` takes them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionSource {
    pub url: String,
    pub tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskIdentity {
    pub version: String,
    /// `None` for a version-only installed receipt
    /// (`brew::version_only_cask_receipt`); compared only when both sides have one.
    pub sha256: Option<String>,
    pub url: Option<String>,
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
            (Self::Cask(a), Self::Cask(b)) => {
                a.version == b.version
                    && both_or_skip(&a.sha256, &b.sha256)
                    && both_or_skip(&a.url, &b.url)
            }
            _ => false,
        }
    }

    /// A cask whose definition says `version :latest`: the artifact behind
    /// it changes without the cask changing, so its contents cannot be soaked.
    pub fn is_latest_cask(&self) -> bool {
        matches!(self, Self::Cask(c) if c.version == ":latest")
    }
}

fn both_or_skip(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// With a `host`, `version`, `url` and `sha256` come from the last definition
/// before `bottle do` inside the platform blocks and `if` branches that apply
/// to it (Ruby's later definition wins, and heroku overrides its x64 url for
/// arm after it). Without one, those blocks stay opaque and the first match wins.
pub fn parse_formula(rb: &str, host: Option<&HostPlatform>) -> Result<FormulaIdentity, Error> {
    let lines = depth1_lines(formula_class(rb), host);
    let last = host.is_some();
    // Homebrew's component order puts `url`/`version`/`sha256` before
    // `bottle do`, `service do`, every `def` and `test do`. A Cellar receipt has
    // no bottle block, so those are what end the identity scope.
    let scope: &[&str] = match lines.iter().position(|t| ends_identity_scope(t)) {
        Some(i) => &lines[..i],
        None => &lines,
    };
    let git_tag = keyword_quoted(scope, "tag:");
    let git_rev = keyword_quoted(scope, "revision:");
    let url = pick_quoted(scope, "url ", last);
    let mut derived_from = None;
    let version = match pick_quoted(scope, "version ", last) {
        Some(v) => v,
        None => {
            if let Some(tag) = git_tag.as_deref() {
                derived_from = url.clone().map(|url| VersionSource {
                    url,
                    tag: Some(tag.to_string()),
                });
                drop_tag_v(tag).to_string()
            } else {
                let url = url.ok_or_else(|| {
                    Error::Other("formula missing version, git tag, and url".into())
                })?;
                let v = version_from_url(&url).ok_or_else(|| {
                    Error::Other("could not derive formula version from url".into())
                })?;
                derived_from = Some(VersionSource { url, tag: None });
                v
            }
        }
    };
    let revision = first_u32_in(depth1_lines(rb, None), "revision ").unwrap_or(0);
    let rebuild = first_u32(rb, "rebuild ");
    let sha256 = pick_quoted(scope, "sha256 ", last)
        .or(git_rev)
        .or(git_tag)
        .ok_or_else(|| Error::Other("formula missing sha256 and git revision".into()))?;
    Ok(FormulaIdentity {
        version,
        revision,
        rebuild,
        sha256,
        derived_from,
    })
}

/// The first (or, with `last`, the last) `key "..."` among `lines`.
fn pick_quoted(lines: &[&str], key: &str, last: bool) -> Option<String> {
    let find = |t: &&str| t.strip_prefix(key).and_then(first_double_quoted);
    if last {
        lines.iter().rev().find_map(find)
    } else {
        lines.iter().find_map(find)
    }
}

/// `tag: "v1.2.3"` / `revision: "abc…"` (Homebrew git-url kwargs), on a
/// continuation line of the `url` statement or after it on the same line.
/// Only lines of `scope` count, so a resource's `url ..., revision:` does not.
fn keyword_quoted(scope: &[&str], key: &str) -> Option<String> {
    scope.iter().find_map(|t| {
        let rest = if t.starts_with("url ") {
            let pos = t.find(key).filter(|&p| t[..p].ends_with([' ', ',']))?;
            &t[pos + key.len()..]
        } else {
            t.strip_prefix(key)?
        };
        first_double_quoted(rest.trim_start())
    })
}

/// The text from the formula's own `class X < Formula` line: helper classes
/// before it (glibc defines requirements first) have `def`s of their own.
fn formula_class(rb: &str) -> &str {
    let mut pos = 0;
    for line in rb.split_inclusive('\n') {
        if line.starts_with("class ") && line.contains("< Formula") {
            return &rb[pos..];
        }
        pos += line.len();
    }
    rb
}

/// A depth-1 line after which no `url`/`version`/`sha256` of the formula can follow.
fn ends_identity_scope(t: &str) -> bool {
    let code = t.split('#').next().unwrap_or(t).trim_end();
    is_bottle_do(code) || code.starts_with("def ") || code == "test do" || code == "service do"
}

/// Top-level `auto_updates true`: the app updates itself, and `brew upgrade`
/// / `brew outdated` skip it unless `--greedy` or it is named explicitly.
pub fn cask_auto_updates(rb: &str, host: Option<&HostPlatform>) -> bool {
    depth1_lines(rb, host).iter().any(|t| {
        t.strip_prefix("auto_updates")
            .is_some_and(|r| r.split('#').next().unwrap_or(r).trim() == "true")
    })
}

/// With a `host`, `version`, `sha256` and `url` inside the platform blocks that
/// apply to it are read; without one, those blocks stay opaque.
pub fn parse_cask(rb: &str, host: Option<&HostPlatform>) -> Result<CaskIdentity, Error> {
    let lines = depth1_lines(rb, host);
    let version = quoted_or_symbol_in(&lines, "version ")
        .ok_or_else(|| Error::Other("cask missing version".into()))?;
    let sha256 = cask_sha256(&lines, host);
    let url = first_quoted_in(lines.iter().copied(), "url ");
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
/// With a `host`, only lines outside platform blocks that do not apply to it
/// count; without one, every line is scanned (refusing is the conservative failure).
pub fn lifecycle_flags(rb: &str, host: Option<&HostPlatform>) -> Vec<LifecycleFlag> {
    let mut out = Vec::new();
    let lines: Vec<&str> = match host {
        Some(_) => depth1_lines(rb, host),
        None => rb.lines().map(str::trim_start).collect(),
    };
    for t in lines {
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
pub fn is_deprecated_or_disabled(rb: &str, today: &str, host: Option<&HostPlatform>) -> bool {
    lifecycle_flags(rb, host)
        .iter()
        .any(|flag| flag_in_effect(flag, today))
}

/// Deprecations more than a year out are not actionable; only warn about the
/// ones close enough to plan around.
pub fn upcoming_lifecycle_messages(
    rb: &str,
    today: &str,
    host: Option<&HostPlatform>,
) -> Vec<String> {
    let horizon = one_year_out(today);
    lifecycle_flags(rb, host)
        .into_iter()
        .filter(|flag| !flag_in_effect(flag, today))
        .filter_map(|flag| {
            let date = flag.date?;
            if horizon.as_deref().is_some_and(|h| date.as_str() > h) {
                return None;
            }
            let word = match flag.kind {
                LifecycleKind::Deprecated => "deprecated",
                LifecycleKind::Disabled => "disabled",
            };
            Some(format!("scheduled to be {word} on {date}"))
        })
        .collect()
}

/// `2026-09-01` -> `2027-09-01`. ISO dates compare lexically, so bumping the
/// year field is enough; Feb 29 lands on a date that never exists, which still
/// orders correctly.
fn one_year_out(today: &str) -> Option<String> {
    if !is_iso_date(today) {
        return None;
    }
    let year: u32 = today[..4].parse().ok()?;
    Some(format!("{:04}{}", year + 1, &today[4..]))
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
/// every other block stay opaque, except that with a `host` a platform block
/// (`on_arm`, `on_monterey :or_newer`, ...) that applies to it is transparent too,
/// as is an `if` whose condition `condition_holds` can decide (and its `else`
/// when the condition fails). `if`, `unless`, `case`, `begin`, `while`, `until`
/// and `def` blocks are tracked so their `end` closes the right entry.
fn depth1_lines<'a>(rb: &'a str, host: Option<&HostPlatform>) -> Vec<&'a str> {
    struct Open {
        transparent: bool,
        /// An `if` chain whose conditions were all recognized so far: `Some(true)`
        /// once a branch has held. An `elsif` is evaluated only while no branch
        /// has held, and an `else` applies only if none did. An unrecognized
        /// condition makes the rest of the chain opaque.
        chain: Option<bool>,
    }
    let mut depth = 0i32;
    let mut open_blocks: Vec<Open> = Vec::new();
    let mut out = Vec::new();
    for line in rb.lines() {
        let t = line.trim_start();
        if is_ruby_end(t) {
            if open_blocks.pop().is_none_or(|o| !o.transparent) {
                depth = depth.saturating_sub(1);
            }
            continue;
        }
        let code = t.split('#').next().unwrap_or(t).trim_end();
        if let Some(top) = open_blocks.last_mut().filter(|o| o.chain.is_some()) {
            let taken = top.chain == Some(true);
            let next = if code == "else" {
                Some((!taken, None))
            } else if let Some(cond) = code.strip_prefix("elsif ") {
                let holds = if taken {
                    Some(false)
                } else {
                    host.and_then(|h| condition_holds(cond, h))
                };
                Some(match holds {
                    Some(h) => (h, Some(taken || h)),
                    None => (false, None),
                })
            } else {
                None
            };
            if let Some((transparent, chain)) = next {
                if transparent != top.transparent {
                    depth += if transparent { -1 } else { 1 };
                }
                top.transparent = transparent;
                top.chain = chain;
                continue;
            }
        }
        let condition = match (host, code.strip_prefix("if ")) {
            (Some(h), Some(cond)) if depth == 1 => condition_holds(cond, h),
            _ => None,
        };
        let transparent = depth == 1
            && (is_stable_do(t)
                || host.is_some_and(|h| h.block_applies(t))
                || condition == Some(true));
        if depth == 1 && !transparent {
            out.push(t);
        }
        if opens_ruby_block(t, open_blocks.is_empty()) {
            open_blocks.push(Open {
                transparent,
                chain: condition,
            });
            if !transparent {
                depth += 1;
            }
        }
    }
    out
}

/// Whether an `if` condition holds on `host`. Only `Hardware::CPU.arm?`,
/// `Hardware::CPU.intel?`, `Hardware::CPU.is_64_bit?` (true on arm64 and
/// x86_64), `OS.mac?` (true), `OS.linux?` (false), `!` and `&&` are known;
/// anything else, including `||`, parentheses and `then`, is `None`.
fn condition_holds(cond: &str, host: &HostPlatform) -> Option<bool> {
    let mut holds = true;
    for term in cond.split("&&") {
        let term = term.trim();
        let (negated, name) = match term.strip_prefix('!') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, term),
        };
        let value = match name {
            "Hardware::CPU.arm?" => host.is_arm(),
            "Hardware::CPU.intel?" => !host.is_arm(),
            "Hardware::CPU.is_64_bit?" | "OS.mac?" => true,
            "OS.linux?" => false,
            _ => return None,
        };
        holds &= value != negated;
    }
    Some(holds)
}

fn is_stable_do(t: &str) -> bool {
    t.split('#').next().unwrap_or(t).trim_end() == "stable do"
}

fn is_ruby_end(t: &str) -> bool {
    t == "end"
        || [
            "end ", "end;", "end#", "end.", "end&.", "end)", "end]", "end,",
        ]
        .iter()
        .any(|p| t.starts_with(p))
}

/// `at_top`: outside every block, where `class` / `module` / `cask` name the
/// definition; deeper they are a `cask "token"` livecheck reference or similar.
fn opens_ruby_block(t: &str, at_top: bool) -> bool {
    if at_top && (t.starts_with("class ") || t.starts_with("module ") || t.starts_with("cask ")) {
        return true;
    }
    let stripped = t.split('#').next().unwrap_or(t).trim_end();
    if stripped == "do" || stripped.ends_with(" do") || stripped.contains(" do |") {
        return true;
    }
    let keyword = stripped == "begin"
        || [
            "if ", "unless ", "case ", "while ", "until ", "for ", "def ",
        ]
        .iter()
        .any(|k| stripped.starts_with(k))
        || opens_as_expression(stripped);
    keyword && !stripped.ends_with(" end") && !is_endless_def(stripped)
}

/// `x = if ...`, `x ||= case ...`, `x += begin`: the keyword right after an
/// assignment operator opens a block that closes with `end`. `foo if bar` and
/// `x = foo if bar` are modifiers and do not match.
fn opens_as_expression(code: &str) -> bool {
    ["if ", "unless ", "case "]
        .iter()
        .any(|k| code.contains(&format!("= {k}")))
        || code.ends_with("= begin")
        || code.contains("= begin ")
}

/// `def name(args) = expr` / `def name = expr`: a method with no `end`.
/// Operator names (`<=>`, `[]=`, `==`) and setters (`foo=(v)`) contain an `=`
/// that is part of the name, not the endless marker.
fn is_endless_def(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("def ") else {
        return false;
    };
    let rest = rest.trim_start();
    const OPERATORS: &[&str] = &[
        "[]=", "<=>", "===", "[]", "==", "=~", "!=", "<<", ">>", "<=", ">=", "**", "+@", "-@",
        "!~", "+", "-", "*", "/", "%", "<", ">", "!", "~", "&", "|", "^",
    ];
    let name_len = if rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        let mut n = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .unwrap_or(rest.len());
        let tail = &rest[n..];
        if tail.starts_with('?') || tail.starts_with('!') {
            n += 1;
        } else if tail.starts_with("=(") {
            // `def foo=(v)` is a setter.
            n += 1;
        }
        n
    } else {
        OPERATORS
            .iter()
            .find(|op| rest.starts_with(**op))
            .map_or(0, |op| op.len())
    };
    let mut after = &rest[name_len..];
    if after.starts_with('(') {
        let Some(close) = after.find(')') else {
            return false;
        };
        after = &after[close + 1..];
    }
    let after = after.trim_start();
    after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=~")
}

/// The cask `sha256`. Besides the plain form it reads `sha256 arm: "x", intel:
/// "y"` (possibly continued on the next line) by the host's architecture; with
/// no host that form falls through to the first quoted string, as before.
fn cask_sha256(lines: &[&str], host: Option<&HostPlatform>) -> Option<String> {
    let Some(host) = host else {
        return quoted_or_symbol_in(lines, "sha256 ");
    };
    for (i, t) in lines.iter().enumerate() {
        let Some(rest) = t.strip_prefix("sha256 ") else {
            continue;
        };
        if !rest
            .trim_start()
            .starts_with(|c: char| c.is_ascii_lowercase())
        {
            return quoted_or_symbol_in(&[*t], "sha256 ");
        }
        let mut stmt = rest.trim_end().to_string();
        let mut j = i + 1;
        while stmt.ends_with(',') && j < lines.len() {
            stmt.push(' ');
            stmt.push_str(lines[j]);
            j += 1;
        }
        let pos = stmt.find(host.arch_keyword())?;
        return first_double_quoted(&stmt[pos..]);
    }
    None
}

/// Quoted string, or a Ruby symbol such as `:latest` / `:no_check`.
fn quoted_or_symbol_in(lines: &[&str], key: &str) -> Option<String> {
    for t in lines {
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

fn version_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let segment = path.rsplit('/').next().filter(|s| !s.is_empty())?;
    let base = strip_alpha_extension(strip_archive_suffix(segment));
    let version = version_from_basename(base);
    Some(drop_tag_v(&version).to_string())
}

/// Homebrew drops the `v` of a tag-style url (`v5.0.2.tar.gz`, `foo-v5.0.2`)
/// from the version, so the keg is `5.0.2`. Only `v` before a digit counts.
fn drop_tag_v(version: &str) -> &str {
    match version.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => version,
    }
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
    fn version_from_url_drops_the_tag_v_as_brew_does() {
        for url in [
            "https://github.com/x/simdjson/archive/refs/tags/v5.0.2.tar.gz",
            "https://example.com/simdjson-v5.0.2.tar.gz",
        ] {
            assert_eq!(version_from_url(url).as_deref(), Some("5.0.2"), "{url}");
        }
        assert_eq!(
            version_from_url("https://example.com/vim-9.1.tar.gz").as_deref(),
            Some("9.1")
        );
        assert_eq!(
            version_from_url("https://example.com/v.tar.gz").as_deref(),
            Some("v")
        );
    }

    #[test]
    fn version_from_url_drops_a_non_archive_extension() {
        // `ca-certificates` ships a bare `.pem`, not a tarball.
        const RB: &str = r#"
class CaCertificates < Formula
  url "https://curl.se/ca/cacert-2026-08-13.pem"
  sha256 "f66dff1bdf8f96060b8177976f8b7d9254bc89bc4db933d769f7384d28480bc9"
end
"#;
        let got = parse_formula(RB, None).expect("parse ca-certificates");
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
        let got = parse_formula(STABLE_BLOCK_RB, None).expect("parse stable block");
        assert_eq!(
            got,
            FormulaIdentity {
                version: "5.3.15".into(),
                revision: 1,
                rebuild: Some(2),
                sha256: "0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba".into(),
                derived_from: None,
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
        let got = parse_formula(RB, None).expect("parse head + stable");
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
        let got = parse_formula(RB, None).expect("parse nested resource");
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
        let id = parse_formula(rb, None).unwrap();
        assert_eq!(id.version, "1.21.4");
        assert_eq!(id.sha256, "aaa111");
    }

    #[test]
    fn parse_formula_from_url_version_and_source_sha() {
        let id = parse_formula(WGET_FORMULA, None).unwrap();
        assert_eq!(id.version, "1.21.4");
        assert_eq!(id.revision, 1);
        assert_eq!(id.rebuild, Some(2));
        assert_eq!(id.sha256, "aaa111");
    }

    #[test]
    fn different_source_sha256_yields_different_identity() {
        let a = parse_formula(WGET_FORMULA, None).unwrap();
        let b = parse_formula(WGET_FORMULA_OTHER_SHA, None).unwrap();
        assert_eq!(a.version, b.version);
        assert_ne!(a, b);
        assert_ne!(PkgIdentity::Formula(a), PkgIdentity::Formula(b));
    }

    #[test]
    fn parse_cask_fields() {
        let id = parse_cask(FOO_CASK, None).unwrap();
        assert_eq!(id.version, "3.0");
        assert_eq!(id.sha256.as_deref(), Some("ccc333"));
        assert_eq!(id.url.as_deref(), Some("https://example.com/foo-3.0.dmg"));
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
        let id = parse_formula(rb, None).unwrap();
        assert_eq!(id.rebuild, None);
        let with_bottle = parse_formula(WGET_FORMULA, None).unwrap();
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
        let id = parse_formula(rb, None).unwrap();
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
        let id = parse_formula(rb, None).unwrap();
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
        let id = parse_cask(rb, None).unwrap();
        assert_eq!(id.version, ":latest");
        assert_eq!(id.sha256.as_deref(), Some(":no_check"));
        assert_eq!(id.url.as_deref(), Some("https://example.com/nightly.dmg"));
    }

    #[test]
    fn version_only_cask_receipt_matches_on_version_alone() {
        // Installed casks on Homebrew >= 4 carry only a version (see
        // `brew::version_only_cask_receipt`); sha256 and url compare only
        // when both sides have them, like a formula's bottle rebuild.
        let installed = parse_cask("cask \"ant\" do\n  version \"1.38.0\"\nend\n", None).unwrap();
        assert_eq!(installed.version, "1.38.0");
        assert_eq!(installed.sha256, None);
        assert_eq!(installed.url, None);
        let full = |v: &str, sha: &str| {
            parse_cask(&format!(
                "cask \"ant\" do\n  version \"{v}\"\n  sha256 \"{sha}\"\n  url \"https://e.com/ant-{v}.zip\"\nend\n"
            ), None)
            .unwrap()
        };
        let cask = PkgIdentity::Cask;
        assert!(cask(installed.clone()).same_artifact(&cask(full("1.38.0", "aaa"))));
        assert!(!cask(installed).same_artifact(&cask(full("1.39.0", "aaa"))));
        assert!(
            !cask(full("1.38.0", "aaa")).same_artifact(&cask(full("1.38.0", "bbb"))),
            "two full receipts still compare sha256"
        );
        let latest = parse_cask("cask \"font-x\" do\n  version :latest\nend\n", None).unwrap();
        assert_eq!(latest.version, ":latest");
    }

    #[test]
    fn cask_auto_updates_reads_the_top_level_stanza() {
        let yes = "cask \"alt-tab\" do\n  version \"11.8.0\"\n  auto_updates true\nend\n";
        let no = "cask \"alt-tab\" do\n  version \"11.8.0\"\nend\n";
        let off = "cask \"alt-tab\" do\n  version \"11.8.0\"\n  auto_updates false\nend\n";
        assert!(cask_auto_updates(yes, None));
        assert!(!cask_auto_updates(no, None));
        assert!(!cask_auto_updates(off, None));
        let commented = "cask \"atemosc\" do\n  auto_updates true # via Sparkle\nend\n";
        assert!(cask_auto_updates(commented, None));
        let commented_off = "cask \"x\" do\n  auto_updates false # true later\nend\n";
        assert!(!cask_auto_updates(commented_off, None));
    }

    #[test]
    fn deprecation_inside_a_year_still_warns() {
        let rb = r#"
class Soon < Formula
  url "https://example.com/soon-1.0.tar.gz"
  sha256 "eee555"
  deprecate! date: "2027-01-15", because: :deprecated_upstream
  disable! date: "2029-01-15", because: :deprecated_upstream
end
"#;
        assert_eq!(
            upcoming_lifecycle_messages(rb, "2026-08-13", None),
            ["scheduled to be deprecated on 2027-01-15"],
            "warn inside a year, stay quiet beyond it"
        );
    }

    #[test]
    fn detects_deprecate_bang() {
        assert!(is_deprecated_or_disabled(
            DEPRECATED_FORMULA,
            "2024-01-01",
            None
        ));
        assert!(is_deprecated_or_disabled(
            DEPRECATED_FORMULA,
            "2026-08-13",
            None
        ));
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
        assert!(!is_deprecated_or_disabled(rb, "2026-08-13", None));
        // Both dates are years out, so neither is worth a warning today.
        assert!(upcoming_lifecycle_messages(rb, "2026-08-13", None).is_empty());
    }

    #[test]
    fn missing_date_is_treated_as_in_effect() {
        let rb = "  deprecate! because: :unmaintained\n";
        assert!(is_deprecated_or_disabled(rb, "2026-08-13", None));
        assert!(upcoming_lifecycle_messages(rb, "2026-08-13", None).is_empty());
    }

    #[test]
    fn comment_disable_does_not_match() {
        let rb = "# disable! maybe\nclass X < Formula\nend\n";
        assert!(!is_deprecated_or_disabled(rb, "2026-08-13", None));
    }

    #[test]
    fn is_latest_cask_reads_only_the_cutoff_side() {
        let latest = PkgIdentity::Cask(
            parse_cask(
                "cask \"nightly\" do\n  version :latest\n  sha256 :no_check\nend\n",
                None,
            )
            .unwrap(),
        );
        let numbered = PkgIdentity::Cask(
            parse_cask(
                "cask \"nightly\" do\n  version \"2.0\"\n  sha256 \"abc\"\nend\n",
                None,
            )
            .unwrap(),
        );
        let formula = PkgIdentity::Formula(FormulaIdentity {
            version: ":latest".into(),
            revision: 0,
            rebuild: None,
            sha256: "s".into(),
            derived_from: None,
        });
        assert!(latest.is_latest_cask());
        assert!(
            !numbered.is_latest_cask(),
            "a cutoff that moved to a number soaks"
        );
        assert!(!formula.is_latest_cask(), "only casks have :latest");
        // brew's version-only receipt for an installed :latest cask.
        let receipt = PkgIdentity::Cask(
            parse_cask(
                &crate::brew::version_only_cask_receipt("nightly", "latest"),
                None,
            )
            .unwrap(),
        );
        assert!(receipt.is_latest_cask());
        assert!(
            receipt.same_artifact(&latest),
            "same definition: already soaked"
        );
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    const NGROK: &str = include_str!("../tests/fixtures/ngrok.rb");
    const WIRESHARK: &str = include_str!("../tests/fixtures/wireshark-app.rb");
    const CYCLONEDX: &str = include_str!("../tests/fixtures/cyclonedx-cli.rb");
    const HEROKU: &str = include_str!("../tests/fixtures/heroku.rb");
    const WS_ARM: &str = "c869615ebbe8f1db1ebf62d503df7011b7c347f4c26997dc28cb4b68db0a4b49";
    const WS_INTEL: &str = "344ec1a1c85f6a46c3446392544dc179a25f94f4fdf7dbcad1159e3e5dc60fba";

    const SYMBOLS: &str = "golden_gate 27\ntahoe 26\nsequoia 15\nsonoma 14\nventura 13\nmonterey 12\nbig_sur 11\ncatalina 10.15\n";

    fn host(macos: &str, arch: &str) -> HostPlatform {
        HostPlatform::parse(&format!("{SYMBOLS}HOST {macos} {arch}\n")).expect("host")
    }

    #[test]
    fn without_a_host_platform_blocks_stay_opaque() {
        for rb in [NGROK, WIRESHARK] {
            let err = parse_cask(rb, None).unwrap_err().to_string();
            assert!(err.contains("cask missing version"), "{err}");
        }
    }

    #[test]
    fn ngrok_version_and_sha_follow_the_architecture() {
        let arm = parse_cask(NGROK, Some(&host("sequoia", "arm64"))).unwrap();
        assert_eq!(arm.version, "3.39.11,dy27whJwwmb,a");
        assert!(arm.sha256.as_deref().unwrap().starts_with("9324a655"));
        let intel = parse_cask(NGROK, Some(&host("sequoia", "x86_64"))).unwrap();
        assert_eq!(intel.version, "3.39.11,8QQF2ciKqxM,a");
        assert!(intel.sha256.as_deref().unwrap().starts_with("c6b9b3d9"));
        assert!(arm.url.unwrap().starts_with("https://bin.ngrok.com/"));
    }

    #[test]
    fn wireshark_on_big_sur_takes_the_old_stable_with_the_arch_sha() {
        let arm = parse_cask(WIRESHARK, Some(&host("big_sur", "arm64"))).unwrap();
        assert_eq!(arm.version, "4.4.17");
        assert_eq!(arm.sha256.as_deref(), Some(WS_ARM));
        assert!(arm.url.unwrap().contains("%20#{arch}%2064.dmg"));
        let intel = parse_cask(WIRESHARK, Some(&host("big_sur", "x86_64"))).unwrap();
        assert_eq!(intel.version, "4.4.17");
        assert_eq!(intel.sha256.as_deref(), Some(WS_INTEL));
    }

    #[test]
    fn wireshark_on_a_newer_macos_takes_the_current_release() {
        for macos in ["monterey", "sequoia", "golden_gate"] {
            let id = parse_cask(WIRESHARK, Some(&host(macos, "arm64"))).unwrap();
            assert_eq!(id.version, "4.6.9", "{macos}");
            assert!(id.sha256.as_deref().unwrap().starts_with("10bd54cc"));
            assert!(id.url.unwrap().ends_with("%20#{version}.dmg"));
        }
    }

    #[test]
    fn exact_and_older_codename_blocks() {
        let rb = "cask \"x\" do\n  on_sonoma do\n    version \"14\"\n  end\n  on_sequoia do\n    version \"15\"\n  end\n  on_ventura :or_older do\n    version \"13\"\n  end\nend\n";
        let v = |m: &str| parse_cask(rb, Some(&host(m, "arm64"))).map(|i| i.version);
        assert_eq!(v("sonoma").unwrap(), "14");
        assert_eq!(v("sequoia").unwrap(), "15");
        assert_eq!(v("ventura").unwrap(), "13");
        assert_eq!(v("catalina").unwrap(), "13");
        assert!(v("tahoe").is_err(), "no block applies");
    }

    #[test]
    fn linux_unknown_codename_and_unknown_forms_stay_opaque() {
        let h = host("sequoia", "arm64");
        for block in [
            "on_linux do",
            "on_nonesuch :or_newer do",
            "on_sequoia :sometimes do",
            "on_system :linux, macos: :big_sur_or_newer do",
        ] {
            let rb = format!("cask \"x\" do\n  {block}\n    version \"9\"\n  end\nend\n");
            assert!(parse_cask(&rb, Some(&h)).is_err(), "{block}");
        }
        let rb = "cask \"x\" do\n  on_macos do\n    version \"9\"\n  end\nend\n";
        assert_eq!(parse_cask(rb, Some(&h)).unwrap().version, "9");
    }

    #[test]
    fn nested_blocks_inside_an_applying_block_stay_opaque() {
        let rb = "cask \"x\" do\n  on_arm do\n    version \"1\"\n    livecheck do\n      url \"https://nope\"\n    end\n  end\n  url \"https://yes\"\nend\n";
        let id = parse_cask(rb, Some(&host("sequoia", "arm64"))).unwrap();
        assert_eq!(id.version, "1");
        assert_eq!(id.url.as_deref(), Some("https://yes"));
    }

    #[test]
    fn sha256_keyword_form_selects_by_arch_at_top_level() {
        let rb = "cask \"x\" do\n  version \"1\"\n  sha256 arm: \"aa\", intel: \"bb\"\nend\n";
        let sha = |arch: &str| parse_cask(rb, Some(&host("sequoia", arch))).unwrap().sha256;
        assert_eq!(sha("arm64").as_deref(), Some("aa"));
        assert_eq!(sha("x86_64").as_deref(), Some("bb"));
        let one = "cask \"x\" do\n  version \"1\"\n  sha256 arm: \"aa\"\nend\n";
        let id = parse_cask(one, Some(&host("sequoia", "x86_64"))).unwrap();
        assert_eq!(id.sha256, None, "no sha for this arch");
    }

    #[test]
    fn auto_updates_inside_an_applying_block() {
        let rb = "cask \"x\" do\n  on_arm do\n    auto_updates true\n  end\nend\n";
        assert!(!cask_auto_updates(rb, None));
        assert!(cask_auto_updates(rb, Some(&host("sequoia", "arm64"))));
        assert!(!cask_auto_updates(rb, Some(&host("sequoia", "x86_64"))));
    }

    #[test]
    fn formula_parsing_ignores_platform_blocks() {
        let rb = "class Foo < Formula\n  url \"https://e.com/foo-1.2.3.tar.gz\"\n  sha256 \"abc\"\n  on_macos do\n    depends_on \"bar\"\n  end\n  bottle do\n    rebuild 2\n    sha256 cellar: :any, arm64_sequoia: \"def\"\n  end\nend\n";
        let id = parse_formula(rb, None).unwrap();
        assert_eq!(
            (id.version.as_str(), id.sha256.as_str(), id.rebuild),
            ("1.2.3", "abc", Some(2))
        );
    }

    #[test]
    fn host_platform_parse_accepts_brew_output() {
        let h = host("golden_gate", "arm64");
        assert_eq!(h.macos, "golden_gate");
        assert_eq!(h.arch, "arm64");
        assert_eq!(h.macos_versions.len(), 8);
        assert_eq!(h.macos_versions[0], ("golden_gate".into(), "27".into()));
    }

    #[test]
    fn host_platform_parse_rejects_garbage() {
        for bad in [
            "",
            "garbage",
            "sequoia 15\n",
            "sequoia 15\nHOST sequoia\n",
            "sequoia 15\nHOST sonoma arm64\n",
            "sequoia 15\nHOST sequoia riscv\n",
            "sequoia fifteen\nHOST sequoia arm64\n",
            "sequoia 15 extra\nHOST sequoia arm64\n",
            "Error: brew ruby failed\n",
        ] {
            assert_eq!(HostPlatform::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_def_body_does_not_close_the_wrong_block() {
        // The `end` of `def install` must not leave `test do` at depth 1.
        let rb = "class Foo < Formula\n  url \"https://e.com/foo-1.2.3.tar.gz\"\n  sha256 \"abc\"\n  def install\n    bin.install \"x\"\n  end\n  test do\n    url \"https://e.com/bad-9.9.9.tar.gz\"\n    sha256 \"bad\"\n  end\nend\n";
        let lines = depth1_lines(rb, None);
        assert!(!lines.iter().any(|l| l.contains("bad")), "{lines:?}");
        let id = parse_formula(rb, None).unwrap();
        assert_eq!((id.version.as_str(), id.sha256.as_str()), ("1.2.3", "abc"));
    }

    #[test]
    fn cyclonedx_is_unparseable_without_a_host() {
        // Every url and sha256 sits inside `on_macos`/`on_linux`.
        assert!(parse_formula(CYCLONEDX, None).is_err());
    }

    #[test]
    fn one_liners_and_endless_defs_open_no_block() {
        let rb = "class Foo < Formula\n  def a = 1\n  def b(x) = x + 1\n  def c; end\n  x = 1 if true\n  if y then z end\n  url \"https://e.com/foo-1.2.3.tar.gz\"\n  sha256 \"abc\"\nend\n";
        let lines = depth1_lines(rb, None);
        assert!(lines.iter().any(|l| l.starts_with("sha256")), "{lines:?}");
    }

    const HEROKU_ARM: &str = "a1bc52e3f0675bff8a3dcf7c821c6ea8056d279d550addc767ca79114087e95e";
    const HEROKU_INTEL: &str = "108487ac8f232f6d43e342cf0e27341b101cb62dfcf6094ba5e96cd7140da9a0";
    const CDX_ARM: &str = "750c148780154833f6401f9067d08c5a4c31567b6ee3c26c062c3a95c62d741c";
    const CDX_INTEL: &str = "0ea92306fc7d7c30ed112a7463781a200585a3bf1cdaf77229181c5aabd1d4b6";

    #[test]
    fn heroku_takes_the_last_applicable_definition() {
        let arm = parse_formula(HEROKU, Some(&host("sequoia", "arm64"))).unwrap();
        assert_eq!(
            (arm.version.as_str(), arm.sha256.as_str()),
            ("11.12.0", HEROKU_ARM)
        );
        let intel = parse_formula(HEROKU, Some(&host("sequoia", "x86_64"))).unwrap();
        assert_eq!(
            (intel.version.as_str(), intel.sha256.as_str()),
            ("11.12.0", HEROKU_INTEL)
        );
        // Unknown host: blocks stay opaque, first match wins.
        let none = parse_formula(HEROKU, None).unwrap();
        assert_eq!(
            (none.version.as_str(), none.sha256.as_str()),
            ("11.12.0", HEROKU_INTEL)
        );
    }

    #[test]
    fn cyclonedx_resolves_if_conditions_against_the_host() {
        let arm = parse_formula(CYCLONEDX, Some(&host("sequoia", "arm64"))).unwrap();
        assert_eq!(
            (arm.version.as_str(), arm.sha256.as_str()),
            ("0.33.1", CDX_ARM)
        );
        let intel = parse_formula(CYCLONEDX, Some(&host("sequoia", "x86_64"))).unwrap();
        assert_eq!(
            (intel.version.as_str(), intel.sha256.as_str()),
            ("0.33.1", CDX_INTEL)
        );
    }

    fn formula_with(body: &str) -> String {
        format!(
            "class Foo < Formula\n  url \"https://e.com/foo-1.0.tar.gz\"\n  sha256 \"base\"\n{body}  bottle do\n    sha256 cellar: :any, arm64_sequoia: \"b\"\n  end\nend\n"
        )
    }

    fn sha_for(body: &str, h: &HostPlatform) -> String {
        parse_formula(&formula_with(body), Some(h)).unwrap().sha256
    }

    #[test]
    fn if_else_picks_the_branch_that_holds() {
        let body = "  if Hardware::CPU.arm?\n    sha256 \"armsha\"\n  else\n    sha256 \"othersha\"\n  end\n";
        assert_eq!(sha_for(body, &host("sequoia", "arm64")), "armsha");
        assert_eq!(sha_for(body, &host("sequoia", "x86_64")), "othersha");
        let neg = "  if !Hardware::CPU.intel? && OS.mac?\n    sha256 \"a\"\n  else\n    sha256 \"b\"\n  end\n";
        assert_eq!(sha_for(neg, &host("sequoia", "arm64")), "a");
        assert_eq!(sha_for(neg, &host("sequoia", "x86_64")), "b");
        let linux = "  if OS.linux?\n    sha256 \"l\"\n  else\n    sha256 \"m\"\n  end\n";
        assert_eq!(sha_for(linux, &host("sequoia", "arm64")), "m");
        let bits = "  if Hardware::CPU.is_64_bit?\n    sha256 \"s\"\n  end\n";
        assert_eq!(sha_for(bits, &host("sequoia", "x86_64")), "s");
        assert_eq!(sha_for(bits, &host("sequoia", "arm64")), "s");
    }

    #[test]
    fn unrecognized_conditions_stay_opaque() {
        let h = host("sequoia", "arm64");
        for body in [
            "  if Hardware::CPU.arm? && foo?\n    sha256 \"x\"\n  end\n",
            "  if Hardware::CPU.arm? || OS.linux?\n    sha256 \"x\"\n  end\n",
            "  if foo?\n    sha256 \"x\"\n  else\n    sha256 \"x\"\n  end\n",
            "  unless OS.linux?\n    sha256 \"x\"\n  end\n",
            "  if OS.linux?\n    sha256 \"x\"\n  elsif Hardware::CPU.arm?\n    sha256 \"holds\"\n  else\n    sha256 \"x\"\n  end\n",
            "  if OS.linux?\n    sha256 \"x\"\n  elsif foo?\n    sha256 \"x\"\n  else\n    sha256 \"x\"\n  end\n",
            "  if OS.linux?\n    sha256 \"x\"\n  elsif Hardware::CPU.intel?\n    sha256 \"x\"\n  else\n    sha256 \"holds\"\n  end\n",
            "  if OS.linux?\n    sha256 \"x\"\n  elsif foo?\n    sha256 \"x\"\n  elsif Hardware::CPU.arm?\n    sha256 \"x\"\n  end\n",
            "  if Hardware::CPU.arm?\n    sha256 \"holds\"\n  elsif OS.mac?\n    sha256 \"x\"\n  else\n    sha256 \"x\"\n  end\n",
        ] {
            let want = if body.contains("\"holds\"") {
                "holds"
            } else {
                "base"
            };
            assert_eq!(sha_for(body, &h), want, "{body}");
        }
    }

    #[test]
    fn formulae_stop_at_bottle_and_ignore_def_bodies() {
        let h = host("sequoia", "arm64");
        let rb = "class Foo < Formula\n  url \"https://e.com/foo-1.0.tar.gz\"\n  sha256 \"base\"\n  def install\n    version \"9.9\"\n    sha256 \"indef\"\n  end\n  bottle do\n    sha256 cellar: :any, arm64_sequoia: \"b\"\n  end\n  on_arm do\n    sha256 \"late\"\n  end\nend\n";
        let got = parse_formula(rb, Some(&h)).unwrap();
        assert_eq!((got.version.as_str(), got.sha256.as_str()), ("1.0", "base"));
    }

    #[test]
    fn deprecate_in_a_foreign_platform_block_is_ignored() {
        let arm = host("sequoia", "arm64");
        let rb = "class Foo < Formula\n  on_linux do\n    deprecate! date: \"2020-01-01\", because: :x\n  end\nend\n";
        assert!(!is_deprecated_or_disabled(rb, "2026-01-01", Some(&arm)));
        assert!(upcoming_lifecycle_messages(rb, "2026-01-01", Some(&arm)).is_empty());
        assert!(is_deprecated_or_disabled(rb, "2026-01-01", None));
        let rb = "class Foo < Formula\n  on_arm do\n    deprecate! date: \"2020-01-01\", because: :x\n  end\nend\n";
        assert!(is_deprecated_or_disabled(rb, "2026-01-01", Some(&arm)));
        assert!(!is_deprecated_or_disabled(
            rb,
            "2026-01-01",
            Some(&host("sequoia", "x86_64"))
        ));
        let future = "class Foo < Formula\n  on_linux do\n    disable! date: \"2026-06-01\", because: :x\n  end\nend\n";
        assert!(upcoming_lifecycle_messages(future, "2026-01-01", Some(&arm)).is_empty());
        assert_eq!(
            upcoming_lifecycle_messages(future, "2026-01-01", None).len(),
            1
        );
    }

    const CATS: &str = include_str!("../tests/fixtures/cats.rb");

    /// endava/tap `cats`: the arm url sits in an `elsif` nested in `if OS.mac?`.
    #[test]
    fn an_elsif_that_holds_supplies_the_identity() {
        let arm = parse_formula(CATS, Some(&host("sequoia", "arm64"))).unwrap();
        assert_eq!(
            arm.sha256,
            "89f5ac4b179ec8b760c26e9293cdb5f403acda8152618be81a4d54289653b788"
        );
        let intel = parse_formula(CATS, Some(&host("sequoia", "x86_64"))).unwrap();
        assert_eq!(
            intel.sha256,
            "44de442d123f2bfa421b5cf78e5fa529d3fe6b8756d15b7cd2a9ab4953a95b2d"
        );
    }

    const APPIUM: &str = include_str!("../tests/fixtures/appium-inspector.rb");
    const GDBM_RECEIPT: &str = include_str!("../tests/fixtures/python-gdbm-receipt.rb");

    #[test]
    fn end_followed_by_a_method_call_still_closes_the_block() {
        let h = host("sequoia", "arm64");
        assert!(is_deprecated_or_disabled(APPIUM, "2026-10-17", Some(&h)));
        assert!(is_deprecated_or_disabled(APPIUM, "2026-10-17", None));
        for end in [
            "end.flatten",
            "end&.flatten",
            "end.compact",
            "end)",
            "end]",
            "end,",
            "end",
        ] {
            let rb = format!(
                "cask \"x\" do\n  livecheck do\n    foo(bar.map do |r|\n      r\n    {end}\n  end\n  disable! date: \"2024-01-01\", because: :x\nend\n"
            );
            let flags = lifecycle_flags(&rb, Some(&h));
            assert_eq!(flags.len(), 1, "{end}");
        }
    }

    fn lifecycle_after(body: &str) -> usize {
        let rb = format!(
            "class Foo < Formula\n  url \"https://e.com/foo-1.0.tar.gz\"\n  sha256 \"a\"\n{body}  deprecate! date: \"2020-01-01\", because: :x\nend\n"
        );
        lifecycle_flags(&rb, Some(&host("sequoia", "arm64"))).len()
    }

    #[test]
    fn expression_openers_and_for_loops_close_with_end() {
        for opener in [
            "x = if OS.mac?",
            "x = case y",
            "x = unless OS.mac?",
            "x = begin",
            "x ||= if OS.mac?",
            "x += if OS.mac?",
            "for x in y",
            "for x in y do",
        ] {
            let body = format!("  def install\n    {opener}\n      1\n    end\n  end\n");
            assert_eq!(lifecycle_after(&body), 1, "{opener}");
        }
    }

    #[test]
    fn modifiers_and_comparisons_open_no_block() {
        for line in [
            "foo if bar",
            "x = foo if bar",
            "x = foo unless bar",
            "x = (a == b)",
            "x = a if b == c",
            "x = if a then b else c end",
        ] {
            let body = format!("  def install\n    {line}\n  end\n");
            assert_eq!(lifecycle_after(&body), 1, "{line}");
        }
    }

    #[test]
    fn a_receipt_without_a_bottle_block_keeps_the_top_level_identity() {
        // `python_include = if OS.mac?` in `def install` and the heredoc line
        // `version = "#{version}"` must not displace the real version.
        for h in [Some(host("sequoia", "arm64")), None] {
            let got = parse_formula(GDBM_RECEIPT, h.as_ref()).unwrap();
            assert_eq!(got.version, "3.13.16", "{h:?}");
            assert!(got.sha256.starts_with("cfac63bd"), "{h:?}");
        }
    }

    #[test]
    fn identity_scope_ends_at_the_first_def_test_or_service() {
        for stop in ["def install\n  end", "test do\n  end", "service do\n  end"] {
            let rb = format!(
                "class Foo < Formula\n  url \"https://e.com/foo-1.0.tar.gz\"\n  sha256 \"base\"\n  {stop}\n  version \"9.9\"\n  sha256 \"late\"\nend\n"
            );
            for h in [Some(host("sequoia", "arm64")), None] {
                let got = parse_formula(&rb, h.as_ref()).unwrap();
                assert_eq!(
                    (got.version.as_str(), got.sha256.as_str()),
                    ("1.0", "base"),
                    "{stop}"
                );
            }
        }
    }

    #[test]
    fn helper_classes_before_the_formula_do_not_end_the_identity_scope() {
        const RB: &str = "class Helper < Requirement\n  def message\n    \"x\"\n  end\nend\n\nclass Foo < Formula\n  url \"https://e.com/foo-1.0.tar.gz\"\n  sha256 \"a\"\nend\n";
        for h in [Some(host("sequoia", "arm64")), None] {
            let got = parse_formula(RB, h.as_ref()).unwrap();
            assert_eq!(got.version, "1.0", "{h:?}");
        }
    }

    #[test]
    fn a_cask_token_inside_livecheck_is_not_a_block() {
        const RB: &str = "cask \"x\" do\n  version \"1\"\n  sha256 \"a\"\n  livecheck do\n    cask \"x-suite\"\n  end\n\n  auto_updates true\n  app \"X.app\"\nend\n";
        for h in [Some(host("sequoia", "arm64")), None] {
            assert!(cask_auto_updates(RB, h.as_ref()), "{h:?}");
        }
    }

    #[test]
    fn operator_method_defs_are_not_endless() {
        for (def, opens) in [
            ("def <=>(other)", true),
            ("def []=(k, v)", true),
            ("def ==(o)", true),
            ("def [](k)", true),
            ("def +(o)", true),
            ("def foo=(v)", true),
            ("def self.foo(a)", true),
            ("def foo?", true),
            ("def foo = 1", false),
            ("def foo(a) = a", false),
            ("def foo() = 1", false),
            ("def self.foo(a) = a", false),
            ("def ==(o) = true", false),
            ("def <=>(o) = 0", false),
        ] {
            assert_eq!(opens_ruby_block(def, true), opens, "{def}");
        }
    }

    #[test]
    fn git_kwargs_on_the_url_line_are_found() {
        const NETPBM: &str = "class Netpbm < Formula\n  url \"https://svn.example.org/stable\", revision: \"5358\"\n  version \"11.02.30\"\n\n  resource \"ug\" do\n    url \"https://svn.example.org/ug\", revision: \"5348\"\n  end\n\n  bottle do\n    sha256 arm64_sequoia: \"b\"\n  end\nend\n";
        for h in [Some(host("sequoia", "arm64")), None] {
            let got = parse_formula(NETPBM, h.as_ref()).unwrap();
            assert_eq!(
                (got.version.as_str(), got.sha256.as_str()),
                ("11.02.30", "5358"),
                "{h:?}"
            );
        }
        const TAGGED: &str = "class Foo < Formula\n  url \"https://example.org/foo.git\", tag: \"v2.3\", revision: \"abc123\"\nend\n";
        let got = parse_formula(TAGGED, None).unwrap();
        assert_eq!(
            (got.version.as_str(), got.sha256.as_str()),
            ("2.3", "abc123")
        );
    }

    #[test]
    fn derived_from_records_the_url_or_tag_only_when_the_version_is_guessed() {
        let from_url = parse_formula(
            "class Foo < Formula\n  url \"https://example.com/foo-sf_19.tar.gz\"\n  sha256 \"a\"\nend\n",
            None,
        )
        .unwrap();
        assert_eq!(
            from_url.derived_from,
            Some(VersionSource {
                url: "https://example.com/foo-sf_19.tar.gz".into(),
                tag: None
            })
        );
        let tagged = parse_formula(
            "class Foo < Formula\n  url \"https://example.org/foo.git\", tag: \"v2.3\", revision: \"abc\"\nend\n",
            None,
        )
        .unwrap();
        assert_eq!(
            tagged.derived_from,
            Some(VersionSource {
                url: "https://example.org/foo.git".into(),
                tag: Some("v2.3".into())
            })
        );
        let explicit = parse_formula(
            "class Foo < Formula\n  url \"https://example.com/download\"\n  version \"8.1\"\n  sha256 \"a\"\nend\n",
            None,
        )
        .unwrap();
        assert_eq!(explicit.derived_from, None);
        assert_eq!(explicit.version, "8.1");
    }

    #[test]
    fn same_artifact_ignores_what_the_version_was_derived_from() {
        let a = parse_formula(
            "class Foo < Formula\n  url \"https://example.com/foo-1.tar.gz\"\n  sha256 \"a\"\nend\n",
            None,
        )
        .unwrap();
        let b = FormulaIdentity {
            derived_from: None,
            ..a.clone()
        };
        assert!(PkgIdentity::Formula(a).same_artifact(&PkgIdentity::Formula(b)));
    }
}
