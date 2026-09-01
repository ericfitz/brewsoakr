//! Line filter that turns Homebrew's chatty install output into one short line
//! per real event, collects caveats for a single block at the end, and keeps a
//! running byte tally. The raw stream still goes to the log file untouched.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    /// Inside the bare-name list under `==> Would install/upgrade ...`.
    SkipList,
    /// Inside `==> Cleanup` and its `Removing:` lines.
    Cleanup,
    /// Inside `==> Caveats` to the end of the run.
    Caveats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Caveat {
    pkg: String,
    lines: Vec<String>,
}

/// Sections brew emits with `==> `. Used to tell a real section header from a
/// package sub-header inside a caveats block (`==> node`).
const SECTION_WORDS: [&str; 9] = [
    "Downloading",
    "Fetching",
    "Pouring",
    "Installing",
    "Upgrading",
    "Reinstalling",
    "Cleanup",
    "Caveats",
    "Would",
];

#[derive(Debug, Default)]
pub struct Filter {
    state: Option<State>,
    /// Package most recently seen installing; names an unlabeled caveats block.
    last_pkg: Option<String>,
    /// Set right after `==> Upgrading x`, when the next indented line is the
    /// `  0.1.0 -> 0.2.0` continuation.
    expect_version_line: bool,
    caveats: Vec<Caveat>,
    /// Cellar version of everything brew reported installing this session,
    /// including transitive dependencies we did not ask for by name.
    installed: BTreeMap<String, String>,
    added_bytes: u64,
    freed_bytes: u64,
}

impl Filter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset per-run line state. Caveats, byte tallies, and the installed map
    /// accumulate across every brew run in the session.
    pub fn start_run(&mut self) {
        self.state = Some(State::Normal);
        self.expect_version_line = false;
    }

    /// Cellar versions of packages brew installed, including transitive deps.
    pub fn installed(&self) -> &BTreeMap<String, String> {
        &self.installed
    }

    pub fn added_bytes(&self) -> u64 {
        self.added_bytes
    }

    pub fn freed_bytes(&self) -> u64 {
        self.freed_bytes
    }

    /// One raw brew line in, at most one line to show the user out.
    pub fn line(&mut self, raw: &str) -> Option<String> {
        let t = raw.trim_end_matches(['\r', '\n']);
        match self.state.unwrap_or(State::Normal) {
            State::Caveats => self.caveat_line(t),
            State::Cleanup => self.cleanup_line(t),
            State::SkipList => self.skip_list_line(t),
            State::Normal => self.normal_line(t),
        }
    }

    fn caveat_line(&mut self, t: &str) -> Option<String> {
        // Caveats run to the end of a brew invocation, but a warning or error
        // means we lost the boundary; leave rather than swallow the rest.
        if t.starts_with("Warning: ") || t.starts_with("Error: ") {
            self.state = Some(State::Normal);
            return self.normal_line(t);
        }
        if let Some(rest) = t.strip_prefix("==> ") {
            let head = rest.split_whitespace().next().unwrap_or("");
            if SECTION_WORDS.contains(&head) {
                self.state = Some(State::Normal);
                return self.normal_line(t);
            }
            self.caveats.push(Caveat {
                pkg: rest.trim().to_string(),
                lines: Vec::new(),
            });
            return None;
        }
        if let Some(block) = self.caveats.last_mut() {
            block.lines.push(t.to_string());
        }
        None
    }

    fn cleanup_line(&mut self, t: &str) -> Option<String> {
        if t.trim().is_empty() {
            return None;
        }
        if t.starts_with("Removing:") {
            self.freed_bytes += trailing_paren_size(t).unwrap_or(0);
            return None;
        }
        self.state = Some(State::Normal);
        self.normal_line(t)
    }

    fn skip_list_line(&mut self, t: &str) -> Option<String> {
        // The list is bare package names; anything decorated ends it.
        if t.trim().is_empty() || t.starts_with("==> ") || t.starts_with(CHECK) {
            self.state = Some(State::Normal);
            return self.normal_line(t);
        }
        None
    }

    fn normal_line(&mut self, t: &str) -> Option<String> {
        let was_expecting_version = self.expect_version_line;
        self.expect_version_line = false;

        if t.trim().is_empty() {
            return None;
        }
        // `  0.10.5 -> 1.0.0` under `==> Upgrading x`: we said this already.
        if was_expecting_version && t.starts_with(' ') && t.contains("->") {
            return None;
        }
        // `To reinstall 1.0.0, run:` / `  brew reinstall x` add nothing.
        if t.starts_with("To reinstall ") || t.trim_start().starts_with("brew reinstall ") {
            return None;
        }
        if is_already_up_to_date(t) || is_outdated_preamble(t) {
            return None;
        }
        if let Some(rest) = t.strip_prefix(CHECK) {
            return self.download_line(rest.trim());
        }
        if let Some(rest) = t.strip_prefix(BEER) {
            return self.poured_line(rest.trim());
        }
        let Some(rest) = t.strip_prefix("==> ") else {
            return Some(t.to_string());
        };
        self.section_line(rest)
    }

    fn section_line(&mut self, rest: &str) -> Option<String> {
        if rest == "Cleanup" {
            self.state = Some(State::Cleanup);
            return None;
        }
        if rest == "Caveats" {
            self.state = Some(State::Caveats);
            let pkg = self.last_pkg.clone().unwrap_or_default();
            self.caveats.push(Caveat {
                pkg,
                lines: Vec::new(),
            });
            return None;
        }
        if rest.starts_with("Would ") {
            self.state = Some(State::SkipList);
            return None;
        }
        if rest.starts_with("Downloading ") || rest.starts_with("Fetching ") {
            return None;
        }
        if let Some(file) = rest.strip_prefix("Pouring ") {
            let (name, version) = split_bottle_file(file.trim());
            self.last_pkg = Some(name.clone());
            return Some(match version {
                Some(v) => format!("  installing {name} {v}"),
                None => format!("  installing {name}"),
            });
        }
        // `Installing dependencies for x: a, b` / `Upgrading x dependency: a`:
        // the Pouring line names each one, so drop the preamble.
        if rest.starts_with("Installing dependencies for ") || rest.contains(" dependency: ") {
            return None;
        }
        if let Some(name) = rest
            .strip_prefix("Upgrading ")
            .or_else(|| rest.strip_prefix("Installing "))
            .or_else(|| rest.strip_prefix("Reinstalling "))
        {
            self.last_pkg = Some(name.trim().to_string());
            self.expect_version_line = true;
            return None;
        }
        Some(rest.to_string())
    }

    /// `Bottle x (1.0.0)` / `Bottle Manifest x (1.0.0)`.
    fn download_line(&mut self, rest: &str) -> Option<String> {
        if rest.starts_with("Bottle Manifest ") {
            return None;
        }
        let body = rest.strip_prefix("Bottle ")?;
        let (name, version) = split_name_paren(body);
        Some(match version {
            Some(v) => format!("  downloading {name} {v}"),
            None => format!("  downloading {name}"),
        })
    }

    /// `/opt/homebrew/Cellar/x/1.0.0: 109 files, 1MB`.
    fn poured_line(&mut self, rest: &str) -> Option<String> {
        let (path, tail) = rest.split_once(':')?;
        let tail = tail.trim();
        if let Some((name, version)) = split_cellar_path(path) {
            self.last_pkg = Some(name.clone());
            self.installed.insert(name, version);
        }
        self.added_bytes += size_after_comma(tail).unwrap_or(0);
        Some(format!("  installed to {path} ({tail})"))
    }

    /// Caveat block for the end of the run: package name, then the lines worth
    /// keeping. Blocks that only announced already-installed shell completions
    /// are dropped; blocks telling the user how to install completions stay.
    pub fn caveat_report(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut last_name = String::new();
        for block in &self.caveats {
            let kept = keep_caveat_lines(&block.lines);
            if kept.is_empty() {
                continue;
            }
            let name = if block.pkg.is_empty() {
                "(unknown)"
            } else {
                block.pkg.as_str()
            };
            // brew emits `==> Caveats` then `==> <pkg>` for the same package.
            if name != last_name {
                out.push(format!("  {name}:"));
                last_name = name.to_string();
            }
            for line in kept {
                out.push(format!("    {}", line.trim_end()));
            }
        }
        if !out.is_empty() {
            out.insert(0, "caveats:".to_string());
        }
        out
    }
}

const CHECK: &str = "\u{2714}\u{fe0e}";
const BEER: &str = "\u{1f37a}";

/// `x 1.0 is already installed but outdated (so it will be upgraded).` —
/// brewsoak prints its own header for the same thing.
fn is_outdated_preamble(t: &str) -> bool {
    t.contains("is already installed but outdated")
}

fn is_already_up_to_date(t: &str) -> bool {
    t.starts_with("Warning: ") && t.ends_with("is already installed and up-to-date.")
}

fn keep_caveat_lines(lines: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut dropping = false;
    for line in lines {
        if is_completions_already_installed(line) {
            dropping = true;
            continue;
        }
        if dropping && (line.trim().is_empty() || line.starts_with(char::is_whitespace)) {
            continue;
        }
        dropping = false;
        if line.trim().is_empty() && kept.is_empty() {
            continue;
        }
        kept.push(line.clone());
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    kept
}

/// "zsh completions and functions have been installed to:" — noise.
/// "To install completions, run: ..." — keep, it asks the user to act.
fn is_completions_already_installed(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("have been installed to")
        && (l.contains("completion") || l.contains("function"))
        && !l.contains("to install")
}

/// `x--1.0.0.arm64_tahoe.bottle.tar.gz` -> ("x", "1.0.0")
fn split_bottle_file(file: &str) -> (String, Option<String>) {
    let Some((name, rest)) = file.split_once("--") else {
        return (file.to_string(), None);
    };
    let version = rest
        .split('.')
        .take_while(|p| !is_arch_part(p))
        .collect::<Vec<_>>();
    if version.is_empty() {
        return (name.to_string(), None);
    }
    (name.to_string(), Some(version.join(".")))
}

/// Bottle files are `<version>.<arch>.bottle.tar.gz`; the arch part is the
/// first dotted component that is not part of a version number.
fn is_arch_part(part: &str) -> bool {
    !part.is_empty() && !part.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// `x (1.0.0)` -> ("x", "1.0.0")
fn split_name_paren(body: &str) -> (String, Option<String>) {
    match body.split_once(" (") {
        Some((name, rest)) => (
            name.trim().to_string(),
            Some(rest.trim_end_matches(')').to_string()),
        ),
        None => (body.trim().to_string(), None),
    }
}

/// `/opt/homebrew/Cellar/x/1.0.0` -> ("x", "1.0.0")
fn split_cellar_path(path: &str) -> Option<(String, String)> {
    let mut parts = path.trim().rsplit('/');
    let version = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some((name, version))
}

/// `109 files, 1MB` -> bytes for `1MB`.
fn size_after_comma(tail: &str) -> Option<u64> {
    parse_size(tail.rsplit(',').next()?.trim())
}

/// `Removing: /path... (19 files, 418.3KB)` -> bytes for `418.3KB`.
fn trailing_paren_size(line: &str) -> Option<u64> {
    let start = line.rfind('(')?;
    let inner = line[start + 1..].trim_end_matches(')');
    parse_size(inner.rsplit(',').next()?.trim())
}

fn parse_size(s: &str) -> Option<u64> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit = s[digits.len()..].trim().to_ascii_uppercase();
    let n: f64 = digits.parse().ok()?;
    let mult = match unit.as_str() {
        "B" | "" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// Cellar versions brew reported installing in one run, read back from the raw
/// captured output. Lets a session notice that a package it still has queued
/// was already brought up to date as somebody else's dependency.
pub fn installed_from_output(bytes: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("/Cellar/") {
            let path = rest.split(':').next().unwrap_or("");
            if let Some((name, version)) = path.split_once('/')
                && !name.contains('/')
                && !version.contains('/')
            {
                out.insert(name.to_string(), version.to_string());
            }
            continue;
        }
        // `Warning: x 1.0.0 is already installed and up-to-date.`
        if let Some(rest) = line.strip_prefix("Warning: ")
            && is_already_up_to_date(line)
        {
            let mut parts = rest.split_whitespace();
            if let (Some(name), Some(version)) = (parts.next(), parts.next()) {
                out.insert(name.to_string(), version.to_string());
            }
        }
    }
    out
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("GB", 1024.0 * 1024.0 * 1024.0),
        ("MB", 1024.0 * 1024.0),
        ("KB", 1024.0),
        ("B", 1.0),
    ];
    let b = bytes as f64;
    for (unit, mult) in UNITS {
        if b >= mult {
            return format!("{:.1}{unit}", b / mult);
        }
    }
    "0B".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str) -> (Vec<String>, Filter) {
        let mut f = Filter::new();
        f.start_run();
        let out = input.lines().filter_map(|l| f.line(l)).collect();
        (out, f)
    }

    #[test]
    fn upgrade_with_deps_collapses_to_one_line_per_step() {
        let input = "\
==> Downloading bottle manifests
\u{2714}\u{fe0e} Bottle Manifest aws-c-auth (1.0.0)
==> Would install 1 formula:
aws-c-auth
==> Would upgrade 6 dependencies for aws-c-auth:
aws-c-common
aws-c-cal
==> Fetching downloads for: aws-c-auth
\u{2714}\u{fe0e} Bottle Manifest aws-c-common (1.0.0)
\u{2714}\u{fe0e} Bottle aws-c-common (1.0.0)
==> Upgrading aws-c-auth
  0.10.5 -> 1.0.0
==> Installing dependencies for aws-c-auth: aws-c-common and aws-c-cal
==> Upgrading aws-c-auth dependency: aws-c-common
==> Pouring aws-c-common--1.0.0.arm64_tahoe.bottle.tar.gz
\u{1f37a}  /opt/homebrew/Cellar/aws-c-common/1.0.0: 109 files, 1MB
==> Cleanup
Removing: /opt/homebrew/Cellar/aws-c-auth/0.10.5... (19 files, 418.3KB)
Warning: aws-c-cal 1.0.0 is already installed and up-to-date.
To reinstall 1.0.0, run:
  brew reinstall aws-c-cal";
        let (out, f) = run(input);
        assert_eq!(
            out,
            vec![
                "  downloading aws-c-common 1.0.0",
                "  installing aws-c-common 1.0.0",
                "  installed to /opt/homebrew/Cellar/aws-c-common/1.0.0 (109 files, 1MB)",
            ],
            "{out:#?}"
        );
        assert_eq!(
            f.installed().get("aws-c-common").map(String::as_str),
            Some("1.0.0")
        );
        assert_eq!(f.added_bytes(), 1024 * 1024);
        assert_eq!(f.freed_bytes(), (418.3 * 1024.0) as u64);
    }

    #[test]
    fn caveats_drop_installed_completions_but_keep_instructions() {
        let input = "\
==> Pouring git-lfs--3.8.0.arm64_tahoe.bottle.tar.gz
\u{1f37a}  /opt/homebrew/Cellar/git-lfs/3.8.0: 82 files, 15.0MB
==> Caveats
zsh completions have been installed to:
  /opt/homebrew/share/zsh/site-functions
==> git-lfs
Update your git config to finish installation:

  $ git lfs install";
        let (_, f) = run(input);
        let report = f.caveat_report();
        assert_eq!(report[0], "caveats:");
        let text = report.join("\n");
        assert!(!text.contains("site-functions"), "{text}");
        assert!(text.contains("git-lfs:"), "{text}");
        assert!(text.contains("$ git lfs install"), "{text}");
    }

    #[test]
    fn caveat_block_with_only_completions_is_dropped() {
        let input = "\
==> Pouring syft--1.51.1.arm64_tahoe.bottle.tar.gz
\u{1f37a}  /opt/homebrew/Cellar/syft/1.51.1: 10 files, 80.6MB
==> Caveats
zsh completions have been installed to:
  /opt/homebrew/share/zsh/site-functions";
        let (_, f) = run(input);
        assert!(f.caveat_report().is_empty(), "{:?}", f.caveat_report());
    }

    #[test]
    fn caveat_telling_user_to_install_completions_is_kept() {
        let input = "\
==> Caveats
To install completions, run:
  brew completions link";
        let (_, f) = run(input);
        let text = f.caveat_report().join("\n");
        assert!(text.contains("To install completions"), "{text}");
    }

    #[test]
    fn unknown_lines_pass_through_without_arrows() {
        let (out, _) = run("==> Something brand new\nplain line");
        assert_eq!(out, vec!["Something brand new", "plain line"]);
    }

    #[test]
    fn bottle_file_with_revision() {
        let (name, version) = split_bottle_file("foo--1.2.3_1.arm64_tahoe.bottle.tar.gz");
        assert_eq!(name, "foo");
        assert_eq!(version.as_deref(), Some("1.2.3_1"));
    }

    #[test]
    fn installed_read_back_from_raw_output() {
        let raw = "\u{1f37a}  /opt/homebrew/Cellar/aws-c-cal/1.0.0: 23 files, 187.6KB\n\
                   Warning: aws-c-common 1.0.0 is already installed and up-to-date.\n";
        let got = installed_from_output(raw.as_bytes());
        assert_eq!(got.get("aws-c-cal").map(String::as_str), Some("1.0.0"));
        assert_eq!(got.get("aws-c-common").map(String::as_str), Some("1.0.0"));
    }

    #[test]
    fn human_size_rounds() {
        assert_eq!(human_size(1024 * 1024), "1.0MB");
        assert_eq!(human_size(0), "0B");
    }
}
