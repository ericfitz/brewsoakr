//! Line filter that turns Homebrew's chatty install output into one short line
//! per real event, collects caveats for a single block at the end, and keeps a
//! running byte tally. The raw stream still goes to the log file untouched.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    /// Inside the bare-name list under `==> Would install/upgrade ...`.
    SkipList,
    /// Inside a no-soak dry run's `==> Would upgrade|install ...` list.
    WouldList(&'static str),
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

/// `text` without ANSI SGR colour sequences (`ESC [ ... m`).
fn strip_ansi(text: &str) -> String {
    if !text.contains('\u{1b}') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&d) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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

/// What a brew run is for; the no-soak run shows brew's own verdicts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunMode {
    /// brewsoak prints its own line per package and drops brew's.
    #[default]
    Soaked,
    /// brew decides: upgrades, already-current packages and dry-run lists show.
    NoSoak,
}

/// Per-run facts the output filter needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunContext {
    pub mode: RunMode,
    /// Soaked: package (lowercase, no tap prefix) -> the version this run
    /// installs, so brew's "newer version is available" warning for it can go.
    pub expected: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Filter {
    state: Option<State>,
    mode: RunMode,
    expected: BTreeMap<String, String>,
    /// No-soak: package named by `==> Upgrading x` whose `upgrading` line is
    /// not shown yet (a cask has no version line to complete it).
    pending_upgrade: Option<String>,
    /// The last line shown in this brew run, for dropping an immediate repeat.
    last_shown: Option<String>,
    /// Package most recently seen installing; names an unlabeled caveats block.
    last_pkg: Option<String>,
    /// Set right after `==> Upgrading x`, when the next indented line is the
    /// `  0.1.0 -> 0.2.0` continuation.
    expect_version_line: bool,
    /// Set right after a dropped `To reinstall x, run:` line; only the line
    /// directly following it is the `  brew reinstall x` command to drop.
    expect_reinstall_cmd: bool,
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
    pub fn start_run(&mut self, ctx: &RunContext) {
        self.state = Some(State::Normal);
        self.mode = ctx.mode;
        self.expected = ctx.expected.clone();
        self.pending_upgrade = None;
        self.expect_version_line = false;
        self.expect_reinstall_cmd = false;
        self.last_shown = None;
    }

    /// End of a brew run: the `upgrading` line still waiting for a version.
    pub fn finish_run(&mut self) -> Option<String> {
        self.pending_upgrade
            .take()
            .map(|name| format!("upgrading {name}"))
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

    /// One raw brew line in, at most one line to show the user out. ANSI
    /// colour is dropped before matching, so brew's coloured `==>` marker is
    /// treated like the plain one, and a line identical to the one shown just
    /// before it is not shown again (brew prints some lines twice). The log
    /// keeps every raw byte; this is only the terminal view.
    pub fn line(&mut self, raw: &str) -> Option<String> {
        let out = self.filter_line(raw)?;
        if self.last_shown.as_deref() == Some(out.as_str()) {
            return None;
        }
        self.last_shown = Some(out.clone());
        Some(out)
    }

    fn filter_line(&mut self, raw: &str) -> Option<String> {
        let plain = strip_ansi(raw.trim_end_matches(['\r', '\n']));
        let t = plain.as_str();
        match self.state.unwrap_or(State::Normal) {
            State::Caveats => self.caveat_line(t),
            State::Cleanup => self.cleanup_line(t),
            State::SkipList => self.skip_list_line(t),
            State::WouldList(verb) => self.would_list_line(t, verb),
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
        // The list is bare package names; anything decorated ends it, and a
        // warning or error must never be swallowed as a list entry.
        if t.trim().is_empty()
            || t.starts_with("==> ")
            || t.starts_with(CHECK)
            || t.starts_with("Warning: ")
            || t.starts_with("Error: ")
        {
            self.state = Some(State::Normal);
            return self.normal_line(t);
        }
        None
    }

    /// Dry-run list lines become `would upgrade name a -> b` (the aligned
    /// columns squeezed, brew's size dropped) or `would install name [ver]`.
    fn would_list_line(&mut self, t: &str, verb: &str) -> Option<String> {
        if t.trim().is_empty()
            || t.starts_with("==> ")
            || t.starts_with(CHECK)
            || t.starts_with("Warning: ")
            || t.starts_with("Error: ")
        {
            self.state = Some(State::Normal);
            return self.normal_line(t);
        }
        let words: Vec<&str> = t
            .split_whitespace()
            .take_while(|w| !w.starts_with('('))
            .collect();
        if verb == "upgrade" && !t.contains(" -> ") {
            return None;
        }
        (!words.is_empty()).then(|| format!("would {verb} {}", words.join(" ")))
    }

    fn normal_line(&mut self, t: &str) -> Option<String> {
        // No-soak: a cask upgrade has no version line, so the `upgrading`
        // line is completed by whatever non-blank line comes next.
        if self.mode == RunMode::NoSoak
            && !t.trim().is_empty()
            && !(self.expect_version_line && t.starts_with(' ') && t.contains("->"))
            && let Some(name) = self.pending_upgrade.take()
        {
            let head = format!("upgrading {name}");
            return Some(match self.normal_line(t) {
                Some(rest) => format!("{head}\n{rest}"),
                None => head,
            });
        }
        let was_expecting_version = self.expect_version_line;
        self.expect_version_line = false;
        let was_expecting_reinstall = self.expect_reinstall_cmd;
        self.expect_reinstall_cmd = false;

        if t.trim().is_empty() {
            return None;
        }
        // `  0.10.5 -> 1.0.0` under `==> Upgrading x`: we said this already.
        if was_expecting_version && t.starts_with(' ') && t.contains("->") {
            return self
                .pending_upgrade
                .take()
                .map(|name| format!("upgrading {name} {}", t.trim()));
        }
        if self.mode == RunMode::NoSoak
            && let Some(shown) = already_current_pair(t)
        {
            return Some(format!("{shown}\n  Already up-to-date."));
        }
        // `To reinstall 1.0.0, run:` / `  brew reinstall x` add nothing. The
        // command is dropped only directly after the hint: brew's rebuild
        // advice also contains a `brew reinstall` line the user needs.
        if t.starts_with("To reinstall ") {
            self.expect_reinstall_cmd = true;
            return None;
        }
        if was_expecting_reinstall && t.trim_start().starts_with("brew reinstall ") {
            return None;
        }
        // brewsoak prints its own "Updating Homebrew" header.
        if matches!(
            t.trim(),
            "==> Updating Homebrew..." | "Updating Homebrew..."
        ) {
            return None;
        }
        if self.is_expected_more_recent(t) {
            return None;
        }
        if is_already_up_to_date(t)
            || is_already_current(t)
            || is_outdated_preamble(t)
            || is_outdated_advice(t)
        {
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

    /// `Warning: cats 15.0.3 is available and more recent than version
    /// 15.0.0.` about the very version this run installs: brewsoak chose that
    /// version on purpose. Any other package or version still shows.
    fn is_expected_more_recent(&self, t: &str) -> bool {
        let Some((token, installing)) = more_recent_warning(t) else {
            return false;
        };
        let name = token.rsplit('/').next().unwrap_or(token).to_lowercase();
        self.expected.get(&name).is_some_and(|v| v == installing)
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
        // `brew update`'s list of what is outdated at HEAD. brewsoak upgrades
        // to the cutoff, not HEAD, so the list contradicts it; drop it.
        if self.mode == RunMode::NoSoak
            && let Some(verb) = would_list_verb(rest)
        {
            self.state = Some(State::WouldList(verb));
            return None;
        }
        // No-soak: `Upgrading 2 outdated packages:` and its name list; each
        // package's own `==> Upgrading x` line follows.
        if self.mode == RunMode::NoSoak && is_outdated_packages_header(rest) {
            self.state = Some(State::SkipList);
            return None;
        }
        if rest.starts_with("Would ")
            || rest.starts_with("Outdated Formulae")
            || rest.starts_with("Outdated Casks")
        {
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
            let name = name.trim().strip_prefix("Cask ").unwrap_or(name.trim());
            self.last_pkg = Some(name.to_string());
            self.expect_version_line = true;
            if self.mode == RunMode::NoSoak {
                if rest.starts_with("Upgrading ") {
                    self.pending_upgrade = Some(name.to_string());
                } else {
                    let verb = if rest.starts_with("Installing ") {
                        "installing"
                    } else {
                        "reinstalling"
                    };
                    return Some(format!("{verb} {name}"));
                }
            }
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

    /// Caveat lines for the end of the run, unindented (the `Caveats` section
    /// indents them): package name, then the lines worth keeping, one level in.
    /// Blocks that only announced already-installed shell completions are
    /// dropped; blocks telling the user how to install completions stay.
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
                out.push(format!("{name}:"));
                last_name = name.to_string();
            }
            for line in kept {
                out.push(format!("  {line}").trim_end().to_string());
            }
        }
        out
    }
}

const CHECK: &str = "\u{2714}\u{fe0e}";
const BEER: &str = "\u{1f37a}";

/// `Would upgrade 1 formula:` / `Would install 2 casks:` / `Would upgrade 10
/// outdated packages` name a list of targets; the verb is `upgrade` or
/// `install`. The `dependencies` lists under them stay dropped.
pub(crate) fn would_list_verb(rest: &str) -> Option<&'static str> {
    let mut words = rest.split_whitespace();
    if words.next() != Some("Would") {
        return None;
    }
    let verb = match words.next()? {
        "upgrade" => "upgrade",
        "install" => "install",
        _ => return None,
    };
    words.next()?.parse::<u32>().ok()?;
    let kind = words.next()?;
    let is_list = kind.starts_with("formula")
        || kind.starts_with("cask")
        || (kind == "outdated" && verb == "upgrade");
    is_list.then_some(verb)
}

/// `Upgrading 2 outdated packages:` (not `Upgrading x`).
fn is_outdated_packages_header(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    words.next() == Some("Upgrading")
        && words.next().is_some_and(|n| n.parse::<u32>().is_ok())
        && words.next() == Some("outdated")
}

/// The `<token> <version>` of either already-installed warning form.
fn already_current_pair(t: &str) -> Option<String> {
    if let Some(head) = already_current_head(t) {
        return Some(head.to_string());
    }
    if !is_already_up_to_date(t) {
        return None;
    }
    let mut parts = t.strip_prefix("Warning: ")?.split_whitespace();
    Some(format!("{} {}", parts.next()?, parts.next()?))
}

/// `x 1.0 is already installed but outdated (so it will be upgraded).` —
/// brewsoak prints its own header for the same thing.
fn is_outdated_preamble(t: &str) -> bool {
    t.contains("is already installed but outdated")
}

/// The advice `brew update` prints under its outdated list:
/// `You have 2 outdated formulae installed.` /
/// `You can upgrade them with brew upgrade` / `or list them with brew outdated.`
fn is_outdated_advice(t: &str) -> bool {
    (t.starts_with("You have ") && t.contains(" outdated ") && t.ends_with(" installed."))
        || (t.starts_with("You can upgrade ") && t.ends_with(" with brew upgrade"))
        || (t.starts_with("or list ") && t.ends_with(" with brew outdated."))
}

/// The `<token>` and `<c>` of `Warning: <token> <v> is available and more
/// recent than version <c>.`; nothing else matches.
fn more_recent_warning(t: &str) -> Option<(&str, &str)> {
    let rest = t.strip_prefix("Warning: ")?.strip_suffix('.')?;
    let (head, installing) = rest.split_once(" is available and more recent than version ")?;
    let mut words = head.split(' ');
    let (token, newer, None) = (words.next()?, words.next()?, words.next()) else {
        return None;
    };
    let plain = |w: &str| !w.is_empty() && !w.contains(char::is_whitespace);
    (plain(token) && plain(newer) && plain(installing)).then_some((token, installing))
}

/// `Warning: user/tap/x 1.0.0 already installed`: newer brew's no-op answer
/// to `brew upgrade` of a package that is already current. Exactly a token
/// and a version before the phrase, so other warnings still show.
fn is_already_current(t: &str) -> bool {
    already_current_head(t).is_some()
}

/// The `<token> <version>` of `Warning: <token> <version> already installed`.
fn already_current_head(t: &str) -> Option<&str> {
    t.strip_prefix("Warning: ")
        .and_then(|rest| rest.strip_suffix(" already installed"))
        .filter(|head| head.split_whitespace().count() == 2 && !head.contains("  "))
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
    installed_versions(bytes, true)
}

/// Like `installed_from_output`, but only `/Cellar/<name>/<ver>` lines count:
/// an "already installed" warning is not evidence brew wrote a keg.
pub fn cellar_installed_from_output(bytes: &[u8]) -> BTreeMap<String, String> {
    installed_versions(bytes, false)
}

fn installed_versions(bytes: &[u8], include_warnings: bool) -> BTreeMap<String, String> {
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
        if include_warnings
            && let Some(rest) = line.strip_prefix("Warning: ")
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
        f.start_run(&RunContext::default());
        let out = input.lines().filter_map(|l| f.line(l)).collect();
        (out, f)
    }

    fn run_expecting(input: &str, expected: &[(&str, &str)]) -> Vec<String> {
        let mut f = Filter::new();
        f.start_run(&RunContext {
            expected: expected
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            ..RunContext::default()
        });
        input.lines().filter_map(|l| f.line(l)).collect()
    }

    const CATS_WARNING: &str =
        "Warning: cats 15.0.3 is available and more recent than version 15.0.0.";

    #[test]
    fn drops_more_recent_for_expected_version() {
        assert!(run_expecting(CATS_WARNING, &[("cats", "15.0.0")]).is_empty());
    }

    #[test]
    fn keeps_more_recent_for_dependency() {
        let line = "Warning: ca-certificates 2026.10.19 is available and more recent than version 2026.10.18.";
        assert_eq!(run_expecting(line, &[("cats", "15.0.0")]), vec![line]);
    }

    #[test]
    fn keeps_more_recent_when_version_differs() {
        assert_eq!(
            run_expecting(CATS_WARNING, &[("cats", "14.0.0")]),
            vec![CATS_WARNING]
        );
    }

    #[test]
    fn drops_more_recent_with_tap_prefix_match() {
        // The tap-qualified token still names the expected package: dropped.
        let line =
            "Warning: endava/tap/Cats 15.0.3 is available and more recent than version 15.0.0.";
        assert!(run_expecting(line, &[("cats", "15.0.0")]).is_empty());
    }

    #[test]
    fn keeps_more_recent_with_different_wording() {
        for line in [
            "Warning: cats 15.0.3 is available and more recent than version 15.0.0",
            "Warning: cats 15.0.3 is available and newer than version 15.0.0.",
            "Warning: cats 15.0.3 is available and more recent than version 15.0.0. Extra",
            "Warning: cats is available and more recent than version 15.0.0.",
            "Warning: cats 15.0.3 is available and more recent than version 15.0.0.0.",
        ] {
            assert_eq!(
                run_expecting(line, &[("cats", "15.0.0")]),
                vec![line],
                "{line}"
            );
        }
    }

    #[test]
    fn more_recent_is_kept_without_an_expected_version() {
        assert_eq!(run(CATS_WARNING).0, vec![CATS_WARNING]);
    }

    fn run_nosoak(input: &str) -> Vec<String> {
        let mut f = Filter::new();
        f.start_run(&RunContext {
            mode: RunMode::NoSoak,
            ..RunContext::default()
        });
        input.lines().filter_map(|l| f.line(l)).collect()
    }

    #[test]
    fn nosoak_already_installed_reported() {
        let out = run_nosoak(
            "Warning: ericfitz/tap/brewsoak 1.5.2 already installed\n\
             Warning: sqdist 0.6.0 is already installed and up-to-date.\n",
        );
        assert_eq!(
            out,
            vec![
                "ericfitz/tap/brewsoak 1.5.2\n  Already up-to-date.",
                "sqdist 0.6.0\n  Already up-to-date."
            ]
        );
    }

    #[test]
    fn soaked_mode_still_drops_already_installed() {
        let (out, _) = run("Warning: ericfitz/tap/brewsoak 1.5.2 already installed\n\
             Warning: sqdist 0.6.0 is already installed and up-to-date.\n");
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn nosoak_upgrade_line() {
        let out = run_nosoak(
            "==> Upgrading ericfitz/tap/agentbus\n  1.15.0 -> 1.16.0\n\
             ==> Pouring agentbus--1.16.0.arm64_sequoia.bottle.tar.gz\n",
        );
        assert_eq!(
            out,
            vec![
                "upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0",
                "  installing agentbus 1.16.0"
            ]
        );
    }

    #[test]
    fn nosoak_cask_upgrade_without_version_line() {
        let out = run_nosoak("==> Upgrading foo\n==> Downloading https://x/y\nsome note\n");
        assert_eq!(out, vec!["upgrading foo", "some note"]);
    }

    #[test]
    fn nosoak_drops_outdated_package_summary_list() {
        let out = run_nosoak(
            "==> Upgrading 1 outdated package:\nericfitz/tap/agentbus 1.15.0 -> 1.16.0\n\
             ==> Upgrading ericfitz/tap/agentbus\n  1.15.0 -> 1.16.0\n",
        );
        assert_eq!(
            out,
            vec!["upgrading ericfitz/tap/agentbus 1.15.0 -> 1.16.0"]
        );
    }

    #[test]
    fn nosoak_dry_run_list() {
        let out = run_nosoak(
            "==> Would upgrade 1 formula:\ncertifi 2026.7.22_1 -> 2026.10.18\n\
             ==> Would upgrade 1 dependency for certifi:\nca-certificates 2026-09-25 -> 2026-10-20\n",
        );
        assert_eq!(out, vec!["would upgrade certifi 2026.7.22_1 -> 2026.10.18"]);
    }

    #[test]
    fn nosoak_dry_run_cask_list() {
        let out = run_nosoak("==> Would upgrade 1 cask:\nfoo 1.0 -> 2.0\n");
        assert_eq!(out, vec!["would upgrade foo 1.0 -> 2.0"]);
    }

    #[test]
    fn soaked_mode_keeps_dropping_would_lists() {
        let (out, _) = run("==> Would upgrade 1 formula:\ncertifi 1 -> 2\n");
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn nosoak_pending_upgrade_is_flushed_at_the_end_of_the_run() {
        let mut f = Filter::new();
        f.start_run(&RunContext {
            mode: RunMode::NoSoak,
            ..RunContext::default()
        });
        assert_eq!(f.line("==> Upgrading foo"), None);
        assert_eq!(f.finish_run().as_deref(), Some("upgrading foo"));
        assert_eq!(f.finish_run(), None);
        f.start_run(&RunContext::default());
        f.line("==> Upgrading foo");
        assert_eq!(f.finish_run(), None, "soaked runs print their own line");
    }

    #[test]
    fn nosoak_install_and_reinstall_get_a_parent_line() {
        let out = run_nosoak(
            "==> Installing cowsay\n\
             ==> Pouring cowsay--3.8.4.arm64_sequoia.bottle.tar.gz\n\
             ==> Reinstalling tree\n\
             ==> Installing Cask foo\n",
        );
        assert_eq!(
            out,
            vec![
                "installing cowsay",
                "  installing cowsay 3.8.4",
                "reinstalling tree",
                "installing foo"
            ]
        );
    }

    #[test]
    fn nosoak_would_install_lists() {
        let out = run_nosoak(
            "==> Would install 1 formula:\ncowsay 3.8.4\n\
             ==> Would install 2 dependencies for cowsay:\nperl\nlibx\n\
             ==> Would install 1 cask:\nrectangle\n",
        );
        assert_eq!(
            out,
            vec!["would install cowsay 3.8.4", "would install rectangle"]
        );
    }

    #[test]
    fn nosoak_would_upgrade_outdated_packages_columns() {
        let out = run_nosoak(
            "==> Downloading bottle manifests\n\
             \u{2714}\u{fe0e} Bottle Manifest gh (2.102.1)\n\
             ==> Would upgrade 2 outdated packages\n\
             endava/tap/cats     15.0.0     -> 15.0.3\n\
             go                  1.27.2     -> 1.27.3      (67.7MB)\n",
        );
        assert_eq!(
            out,
            vec![
                "would upgrade endava/tap/cats 15.0.0 -> 15.0.3",
                "would upgrade go 1.27.2 -> 1.27.3"
            ]
        );
    }

    #[test]
    fn drops_updating_homebrew_banner() {
        let (out, _) = run("==> Updating Homebrew...\nAlready up-to-date.\n");
        assert_eq!(out, vec!["Already up-to-date.".to_string()]);
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
    fn rebuild_hint_keeps_its_reinstall_command() {
        let input = "\
Warning: ada-url has broken dynamic library links:
  
Rebuild this from source with:
  brew reinstall --build-from-source ada-url
If that's successful, file an issue.";
        let (out, _) = run(input);
        assert_eq!(
            out,
            vec![
                "Warning: ada-url has broken dynamic library links:",
                "Rebuild this from source with:",
                "  brew reinstall --build-from-source ada-url",
                "If that's successful, file an issue.",
            ],
            "{out:#?}"
        );
    }

    #[test]
    fn reinstall_command_only_dropped_right_after_the_hint() {
        let input = "\
To reinstall 1.0.0, run:
  brew reinstall x
  brew reinstall y";
        let (out, _) = run(input);
        assert_eq!(out, vec!["  brew reinstall y"], "{out:#?}");
    }

    #[test]
    fn warning_inside_would_install_list_ends_the_list() {
        let input = "\
==> Would install 2 formulae:
aws-c-auth
Warning: something went wrong
Error: and then this
other-name";
        let (out, _) = run(input);
        assert_eq!(
            out,
            vec![
                "Warning: something went wrong",
                "Error: and then this",
                "other-name",
            ],
            "{out:#?}"
        );
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
        assert_eq!(
            report,
            vec![
                "git-lfs:",
                "  Update your git config to finish installation:",
                "",
                "    $ git lfs install"
            ]
        );
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
    fn brew_update_outdated_list_and_advice_are_dropped() {
        let input = "\
Updated 2 taps (endava/tap and homebrew/cask).
==> New Casks
rew: Record, visualise and measure audio
==> Outdated Formulae
cats
certifi
==> Outdated Casks
some-app

You have 2 outdated formulae and 1 outdated cask installed.
You can upgrade them with brew upgrade
or list them with brew outdated.
The 7.0.10 changelog can be found at:
";
        let (out, _) = run(input);
        assert_eq!(
            out,
            vec![
                "Updated 2 taps (endava/tap and homebrew/cask).",
                "New Casks",
                "rew: Record, visualise and measure audio",
                "The 7.0.10 changelog can be found at:",
            ]
        );
    }

    #[test]
    fn already_installed_no_op_warnings_are_dropped() {
        let input = "\
Warning: ericfitz/tap/agentbus 1.14.3 already installed
Warning: python@3.15 3.15.0 already installed
Warning: libx11 1.8.14 is already installed and up-to-date.
Warning: foo is already installed but is not linked
Warning: something else already installed elsewhere
";
        let (out, _) = run(input);
        assert_eq!(
            out,
            vec![
                "Warning: foo is already installed but is not linked",
                "Warning: something else already installed elsewhere",
            ]
        );
    }

    #[test]
    fn outdated_list_never_swallows_a_warning() {
        let (out, _) = run("==> Outdated Formulae\ncats\nWarning: something\n");
        assert_eq!(out, vec!["Warning: something"]);
    }

    #[test]
    fn unknown_lines_pass_through_without_arrows() {
        let (out, _) = run("==> Something brand new\nplain line");
        assert_eq!(out, vec!["Something brand new", "plain line"]);
    }

    #[test]
    fn ansi_wrapped_marker_is_stripped_like_a_plain_one() {
        let plain = run("==> Verifying checksum for 'x.tar.gz'").0;
        let coloured =
            run("\u{1b}[34m==>\u{1b}[0m \u{1b}[1mVerifying checksum for 'x.tar.gz'\u{1b}[0m").0;
        assert_eq!(plain, vec!["Verifying checksum for 'x.tar.gz'"]);
        assert_eq!(coloured, plain);
    }

    #[test]
    fn ansi_wrapped_section_words_are_still_recognised() {
        let (out, _) = run("\u{1b}[34m==>\u{1b}[0m \u{1b}[1mDownloading https://x\u{1b}[0m");
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_line_identical_to_the_one_just_shown_is_not_shown_again() {
        let (out, _) = run("==> Verifying checksum for 'x'\n\
             \u{1b}[34m==>\u{1b}[0m \u{1b}[1mVerifying checksum for 'x'\u{1b}[0m\n\
             plain\nplain\nother");
        assert_eq!(out, vec!["Verifying checksum for 'x'", "plain", "other"]);
    }

    #[test]
    fn a_new_run_may_repeat_the_previous_runs_last_line() {
        let mut f = Filter::new();
        f.start_run(&RunContext::default());
        assert!(f.line("same").is_some());
        assert!(f.line("same").is_none());
        f.start_run(&RunContext::default());
        assert!(f.line("same").is_some());
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
